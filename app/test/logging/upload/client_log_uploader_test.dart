import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:bubbleplay_app/src/feedback/outbox.dart';
import 'package:bubbleplay_app/src/feedback/report.dart';
import 'package:bubbleplay_app/src/logging/app_log.dart';
import 'package:bubbleplay_app/src/logging/log_store.dart';
import 'package:bubbleplay_app/src/logging/upload/client_log_uploader.dart';
import 'package:bubbleplay_app/src/logging/upload/log_upload_transport.dart';
// fake_async comes with flutter_test; the pubspec is out of this lane's scope.
// ignore: depend_on_referenced_packages
import 'package:fake_async/fake_async.dart';
import 'package:flutter_test/flutter_test.dart';

typedef Json = Map<String, Object?>;

/// The contract fixture: the repo's original when the tree is reachable, else the copy beside
/// this test (linux01-run ships only `app/`).
Map<String, Json> loadContract() {
  File? original;
  var dir = Directory.current.absolute;
  for (var i = 0; i < 5; i++) {
    final f = File('${dir.path}/fixtures/client-log-upload-contract.json');
    if (f.existsSync()) {
      original = f;
      break;
    }
    if (dir.parent.path == dir.path) break;
    dir = dir.parent;
  }
  final copy = File('test/logging/upload/client-log-upload-contract.json');
  final doc = jsonDecode((original ?? copy).readAsStringSync()) as Map;
  return {
    for (final m in doc['messages'] as List)
      (m as Map)['name'] as String: (m['json'] as Map).cast<String, Object?>(),
  };
}

class FakeTransport implements LogUploadTransport {
  final StreamController<Json> _in = StreamController<Json>.broadcast();
  final List<Json> sent = [];
  bool throwOnSend = false;

  @override
  int bufferedBytes = 0;

  @override
  Duration sinceLastInput = const Duration(seconds: 10);

  @override
  Stream<Json> get messages => _in.stream;

  @override
  void send(Json message) {
    if (throwOnSend) throw StateError('closed');
    // What goes out must survive JSON.
    sent.add((jsonDecode(jsonEncode(message)) as Map).cast<String, Object?>());
  }

  void receive(Json m) => _in.add(m);

  Future<void> close() => _in.close();

  List<Json> ofType(String type) => [
        for (final m in sent)
          if (m['type'] == type) m,
      ];
}

/// An in-memory [LogLineSource] with the same cursor rules as [LogStore.page].
class FakeSource implements LogLineSource {
  final Map<String?, List<String>> _byPc = {};
  int _epoch = 0;
  bool throwing = false;

  @override
  int get writeEpoch => _epoch;

  void add(String ts, {String? pc, String src = 'app-win', String msg = 'm', int pad = 0}) {
    _byPc.putIfAbsent(pc, () => []).add(jsonEncode({
          'ts': ts,
          'level': 'info',
          'src': src,
          'device': null,
          'pc': pc,
          'session': null,
          'target': 'T',
          'msg': msg + 'x' * pad,
          'fields': <String, Object?>{},
        }));
    _epoch++;
  }

  static String tsOf(int i) {
    final t = DateTime.utc(2026, 11, 2, 13).add(Duration(milliseconds: i));
    return '${t.toIso8601String().substring(0, 23)}Z';
  }

  void addMany(int n, {String? pc, String src = 'app-win', int from = 0, int pad = 0}) {
    for (var i = from; i < from + n; i++) {
      add(tsOf(i), pc: pc, src: src, msg: 'line$i', pad: pad);
    }
  }

  @override
  Future<LogPage> linesFor(String pcId, {String? afterCursor, int limit = 500}) async =>
      _page(pcId, afterCursor, limit);

  @override
  Future<LogPage> miscLines({String? afterCursor, int limit = 500}) async =>
      _page(null, afterCursor, limit);

  LogPage _page(String? pc, String? afterCursor, int limit) {
    if (throwing) throw StateError('disk');
    final lines = [...?_byPc[pc]]..sort((a, b) {
        final c = (jsonDecode(a) as Map)['ts'].toString().compareTo((jsonDecode(b) as Map)['ts'].toString());
        return c;
      });
    String? cTs;
    var cCount = 0;
    if (afterCursor != null) {
      final i = afterCursor.lastIndexOf('#');
      cTs = afterCursor.substring(0, i);
      cCount = int.parse(afterCursor.substring(i + 1));
    }
    final out = <String>[];
    String? lastTs;
    var seen = 0;
    String? outTs;
    var outCount = 0;
    var more = false;
    for (final l in lines) {
      final ts = (jsonDecode(l) as Map)['ts'] as String;
      if (ts == lastTs) {
        seen++;
      } else {
        lastTs = ts;
        seen = 1;
      }
      if (cTs != null) {
        final c = ts.compareTo(cTs);
        if (c < 0 || (c == 0 && seen <= cCount)) continue;
      }
      if (out.length >= limit) {
        more = true;
        break;
      }
      out.add(l);
      outTs = ts;
      outCount = seen;
    }
    if (out.isEmpty) return LogPage(const [], afterCursor);
    return LogPage(out, '$outTs#${outCount.toString().padLeft(6, '0')}', more: more);
  }
}

Json stateMsg(Json fixture, {Json cursors = const {}, int? interval}) {
  final m = (jsonDecode(jsonEncode(fixture)) as Map).cast<String, Object?>();
  m['cursors'] = cursors;
  if (interval != null) (m['limits']! as Map)['minBatchIntervalMs'] = interval;
  return m;
}

Json ack(Json fixture, int id, {String? cursor}) {
  final m = (jsonDecode(jsonEncode(fixture)) as Map).cast<String, Object?>();
  m['id'] = id;
  m['stream'] = stream;
  if (cursor != null) m['cursor'] = cursor;
  return m;
}

const stream = 'app-win';

ClientLogUploader makeUploader(
  FakeTransport t,
  LogLineSource src, {
  Outbox? outbox,
  bool feedback = false,
  String pc = 'pc-1',
}) =>
    ClientLogUploader(
      transport: t,
      appLog: src,
      outbox: outbox ?? Outbox(Directory('${Directory.systemTemp.path}/bp-no-outbox-dir')),
      stream: stream,
      pcId: pc,
      sendFeedback: feedback,
    );

void main() {
  final contract = loadContract();
  final state = contract['logs-state']!;
  final okAck = contract['logs-ack-ok']!;
  final limitedAck = contract['logs-ack-rate-limited']!;
  final budgetAck = contract['logs-ack-budget']!;
  final rejectedAck = contract['logs-ack-rejected']!;

  test('the local copy of the contract matches the repo original', () {
    final copy = File('test/logging/upload/client-log-upload-contract.json');
    var dir = Directory.current.absolute;
    for (var i = 0; i < 5; i++) {
      final f = File('${dir.path}/fixtures/client-log-upload-contract.json');
      if (f.existsSync()) {
        expect(jsonDecode(copy.readAsStringSync()), jsonDecode(f.readAsStringSync()));
        return;
      }
      dir = dir.parent;
    }
  });

  group('logs (fake async)', () {
    test('waits for logs-state, then sends history with the contract keys', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(3, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        async.elapse(const Duration(seconds: 5));
        expect(t.sent, isEmpty);
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        final batch = t.ofType('logs-batch').single;
        expect(batch.keys.toSet(), (contract['logs-batch']!).keys.toSet());
        expect(batch['v'], 1);
        expect(batch['stream'], stream);
        expect(batch['id'], 1);
        expect(batch['from'], '0');
        expect(batch['live'], false);
        expect((batch['lines'] as List).length, 3);
        expect(batch['to'], '${FakeSource.tsOf(2)}#000001');
        up.dispose();
      });
    });

    test('resumes from the cursor of logs-state, also inside one millisecond', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource();
        for (var i = 0; i < 4; i++) {
          src.add(FakeSource.tsOf(1), pc: 'pc-1', msg: 'same$i');
        }
        src.addMany(2, pc: 'pc-1', from: 5);
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state, cursors: {stream: '${FakeSource.tsOf(1)}#000002'}));
        async.elapse(const Duration(milliseconds: 20));
        final lines = (t.ofType('logs-batch').single['lines'] as List).cast<String>();
        expect(lines.length, 4);
        expect((jsonDecode(lines.first) as Map)['msg'], 'same2');
        expect(t.ofType('logs-batch').single['from'], '${FakeSource.tsOf(1)}#000002');
        up.dispose();
      });
    });

    test('only this stream and this PC (plus lines without a PC), merged by ts', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()
          ..add(FakeSource.tsOf(1), pc: 'pc-1', msg: 'a')
          ..add(FakeSource.tsOf(2), pc: 'pc-2', msg: 'other pc')
          ..add(FakeSource.tsOf(3), pc: 'pc-1', src: 'app-android-core', msg: 'other stream')
          ..add(FakeSource.tsOf(2), msg: 'misc')
          ..add(FakeSource.tsOf(4), pc: 'pc-1', msg: 'b');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        final msgs = [
          for (final l in (t.ofType('logs-batch').single['lines'] as List))
            (jsonDecode(l as String) as Map)['msg'],
        ];
        expect(msgs, ['a', 'misc', 'b']);
        up.dispose();
      });
    });

    test('one batch in flight until its ack', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(250, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        expect(t.ofType('logs-batch').length, 1);
        // No ack: nothing else goes out, however long we wait (until the ack timeout).
        async.elapse(const Duration(seconds: 20));
        expect(t.ofType('logs-batch').length, 1);
        up.dispose();
      });
    });

    test('the ack releases the next batch only after minBatchIntervalMs', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(250, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        final first = t.ofType('logs-batch').first;
        t.receive(ack(okAck, 1, cursor: first['to']! as String));
        async.elapse(const Duration(milliseconds: 100));
        expect(t.ofType('logs-batch').length, 1, reason: 'minBatchIntervalMs not over yet');
        async.elapse(const Duration(milliseconds: 500));
        expect(t.ofType('logs-batch').length, 2);
        final second = t.ofType('logs-batch')[1];
        expect(second['id'], 2);
        expect(second['from'], first['to']);
        up.dispose();
      });
    });

    test('batches stay within 100 lines and 16 KiB', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()
          ..addMany(250, pc: 'pc-1')
          ..addMany(30, pc: 'pc-1', from: 1000, pad: 3000);
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state, interval: 0));
        var seen = 0;
        for (var i = 0; i < 40; i++) {
          async.elapse(const Duration(milliseconds: 50));
          final batches = t.ofType('logs-batch');
          if (batches.length == seen) break;
          seen = batches.length;
          final b = batches.last;
          expect(b['id'], seen);
          t.receive(ack(okAck, seen, cursor: b['to']! as String));
        }
        var total = 0;
        for (final b in t.ofType('logs-batch')) {
          final n = (b['lines'] as List).length;
          total += n;
          expect(n, lessThanOrEqualTo(100));
          expect(utf8.encode(jsonEncode(b)).length, lessThanOrEqualTo(16384));
        }
        expect(total, 280);
        expect(t.ofType('logs-batch').length, greaterThan(3));
        up.dispose();
      });
    });

    test('live lines follow history in one batch per interval, marked live', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(2, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        expect(t.ofType('logs-batch').single['live'], false);
        t.receive(ack(okAck, 1, cursor: t.ofType('logs-batch').single['to']! as String));
        async.elapse(const Duration(seconds: 3));
        expect(t.ofType('logs-batch').length, 1, reason: 'nothing new');
        src
          ..add(FakeSource.tsOf(50), pc: 'pc-1', msg: 'live1')
          ..add(FakeSource.tsOf(51), pc: 'pc-1', msg: 'live2');
        async.elapse(const Duration(seconds: 1));
        final live = t.ofType('logs-batch');
        expect(live.length, 2);
        expect(live[1]['live'], true);
        expect((live[1]['lines'] as List).length, 2);
        up.dispose();
      });
    });

    test('rate-limited: the same batch again after retryAfterMs, not before', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(2, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        t.receive(ack(limitedAck, 1));
        async.elapse(const Duration(milliseconds: 1999));
        expect(t.ofType('logs-batch').length, 1);
        async.elapse(const Duration(milliseconds: 5));
        final all = t.ofType('logs-batch');
        expect(all.length, 2);
        expect(all[1], all[0]);
        up.dispose();
      });
    });

    test('ok acks resume from the cursor the host names', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(5, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        // The host already held more than we sent: it names a later cursor.
        t.receive(ack(okAck, 1, cursor: '${FakeSource.tsOf(3)}#000001'));
        async.elapse(const Duration(seconds: 1));
        expect(up.cursor, '${FakeSource.tsOf(3)}#000001');
        final all = t.ofType('logs-batch');
        expect(all.length, 2);
        expect(all[1]['from'], '${FakeSource.tsOf(3)}#000001');
        expect((all[1]['lines'] as List).length, 1);
        up.dispose();
      });
    });

    test('budget stops everything until the next connection', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(150, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        t.receive(ack(budgetAck, 1));
        src.add(FakeSource.tsOf(999), pc: 'pc-1');
        async.elapse(const Duration(minutes: 5));
        expect(t.ofType('logs-batch').length, 1);
        expect(up.isStopped, isTrue);
        expect(up.stopReason, 'budget');
        expect(async.pendingTimers, isEmpty);
        up.dispose();
      });
    });

    test('rejected: the batch is not retried, the queue moves on', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(150, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        final first = t.ofType('logs-batch').single;
        t.receive(ack(rejectedAck, 1));
        async.elapse(const Duration(seconds: 1));
        expect(up.lastRejectReason, 'batch too large');
        final all = t.ofType('logs-batch');
        expect(all.length, 2);
        expect(all[1]['from'], first['to']);
        expect(all[1]['id'], 2);
        up.dispose();
      });
    });

    test('acks for other streams or ids and messages of unknown version are ignored', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(150, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive({...stateMsg(state), 'v': 2});
        async.elapse(const Duration(seconds: 1));
        expect(t.sent, isEmpty);
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        t.receive({...ack(okAck, 1), 'stream': 'other'});
        t.receive(ack(okAck, 99));
        t.receive({...ack(okAck, 1), 'v': 3});
        t.receive({'type': 'mystery', 'v': 1});
        async.elapse(const Duration(seconds: 2));
        expect(t.ofType('logs-batch').length, 1);
        expect(up.isStopped, isFalse);
        up.dispose();
      });
    });

    test('priority: a full send buffer or recent input holds a batch back', () {
      fakeAsync((async) {
        final t = FakeTransport()
          ..bufferedBytes = 16385
          ..sinceLastInput = const Duration(milliseconds: 100);
        final src = FakeSource()..addMany(2, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(seconds: 2));
        expect(t.sent, isEmpty, reason: 'buffer above 16 KiB');
        t.bufferedBytes = 16384;
        async.elapse(const Duration(seconds: 2));
        expect(t.sent, isEmpty, reason: 'input 100 ms ago');
        t.sinceLastInput = const Duration(milliseconds: 249);
        async.elapse(const Duration(seconds: 2));
        expect(t.sent, isEmpty, reason: 'input 249 ms ago');
        t.sinceLastInput = const Duration(milliseconds: 250);
        async.elapse(const Duration(seconds: 1));
        expect(t.ofType('logs-batch').length, 1);
        up.dispose();
      });
    });

    test('priority also gates the rate-limited resend', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(2, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        t.bufferedBytes = 1 << 20;
        t.receive(ack(limitedAck, 1));
        async.elapse(const Duration(seconds: 10));
        expect(t.ofType('logs-batch').length, 1);
        t.bufferedBytes = 0;
        async.elapse(const Duration(seconds: 1));
        expect(t.ofType('logs-batch').length, 2);
        up.dispose();
      });
    });

    test('never throws: a failing send or a failing source only stops the uploader', () {
      fakeAsync((async) {
        final t = FakeTransport()..throwOnSend = true;
        final src = FakeSource()..addMany(2, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(seconds: 1));
        expect(up.isStopped, isTrue);
        expect(up.stopReason, 'send failed');
        up.dispose();

        final t2 = FakeTransport();
        final bad = FakeSource()
          ..addMany(2, pc: 'pc-1')
          ..throwing = true;
        final up2 = makeUploader(t2, bad)..start();
        t2.receive(stateMsg(state));
        async.elapse(const Duration(seconds: 1));
        expect(t2.sent, isEmpty);
        expect(up2.isStopped, isTrue);
        up2.dispose();
      });
    });

    test('a lost ack stops the uploader; a closed connection too', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(2, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(seconds: 31));
        expect(up.stopReason, 'ack timeout');
        expect(async.pendingTimers, isEmpty);
        up.dispose();

        final t2 = FakeTransport();
        final up2 = makeUploader(t2, FakeSource())..start();
        t2.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        unawaited(t2.close());
        async.elapse(const Duration(milliseconds: 20));
        expect(up2.stopReason, 'connection closed');
        up2.dispose();
      });
    });

    test('dispose cancels timers and stops all sending', () {
      fakeAsync((async) {
        final t = FakeTransport();
        final src = FakeSource()..addMany(250, pc: 'pc-1');
        final up = makeUploader(t, src)..start();
        t.receive(stateMsg(state));
        async.elapse(const Duration(milliseconds: 20));
        expect(async.pendingTimers, isNotEmpty);
        up.dispose();
        up.dispose();
        expect(async.pendingTimers, isEmpty);
        t.receive(ack(okAck, 1, cursor: '${FakeSource.tsOf(99)}#000001'));
        src.add(FakeSource.tsOf(500), pc: 'pc-1');
        async.elapse(const Duration(minutes: 1));
        expect(t.ofType('logs-batch').length, 1);
      });
    });
  });

  group('feedback (real files)', () {
    late Directory tmp;
    late Outbox outbox;

    setUp(() {
      tmp = Directory.systemTemp.createTempSync('bp-upload-');
      outbox = Outbox.forRoot(tmp);
    });

    tearDown(() {
      try {
        tmp.deleteSync(recursive: true);
      } catch (_) {}
    });

    Future<void> until(bool Function() cond, {int ms = 5000}) async {
      final end = DateTime.now().add(Duration(milliseconds: ms));
      while (!cond() && DateTime.now().isBefore(end)) {
        await Future<void>.delayed(const Duration(milliseconds: 5));
      }
      expect(cond(), isTrue, reason: 'timed out');
    }

    Future<OutboxItem> queue(String id, {Uint8List? png, String? pc = 'pc-1'}) {
      final created = DateTime.utc(2026, 11, 2, 13, 50);
      return outbox.add(
        FeedbackReport(id: id, created: created, json: {
          'version': 1,
          'id': id,
          'created': '2026-11-02T13:50:00.000Z',
          'text': 'Stream froze',
          'kind': 'bug',
          'pc': pc,
          'logs': <Object?>[],
          'errors': <Object?>[],
          'screenshot': png == null ? null : Outbox.screenshotName(id, created),
        }),
        screenshot: png,
      );
    }

    bool inOutbox(String id) => tmp.existsSync() &&
        Directory('${tmp.path}/outbox').existsSync() &&
        Directory('${tmp.path}/outbox').listSync().any((e) => e.path.contains(id));

    test('a report with a PNG goes as begin and chunks; markSent only after the ok', () async {
      final png = Uint8List.fromList(List.generate(70000, (i) => i % 251));
      await queue('rep-1', png: png);
      final t = FakeTransport();
      final up = makeUploader(t, FakeSource(), outbox: outbox, feedback: true)..start();
      t.receive(stateMsg(state, interval: 0));
      await until(() => t.ofType('feedback-chunk').length == 3);
      final begin = t.ofType('feedback-begin').single;
      expect(begin.keys.toSet(), contract['feedback-begin']!.keys.toSet());
      expect(begin['id'], 'rep-1');
      expect(begin['pngBytes'], 70000);
      expect(begin['chunks'], 3);
      expect((begin['report']! as Map)['screenshot'], isNull);
      final chunks = t.ofType('feedback-chunk');
      final bytes = BytesBuilder();
      for (var i = 0; i < chunks.length; i++) {
        expect(chunks[i].keys.toSet(), contract['feedback-chunk']!.keys.toSet());
        expect(chunks[i]['i'], i);
        final data = base64Decode(chunks[i]['data']! as String);
        expect(data.length, lessThanOrEqualTo(32768));
        bytes.add(data);
      }
      expect(bytes.toBytes(), png);
      // Sent but not acknowledged: still in the outbox.
      await Future<void>.delayed(const Duration(milliseconds: 100));
      expect(inOutbox('rep-1'), isTrue);
      t.receive({...contract['feedback-ack-ok']!, 'id': 'rep-1'});
      await until(() => !inOutbox('rep-1'));
      up.dispose();
    });

    test('a report without a PNG: pngBytes 0, chunks 0, and no chunk messages', () async {
      await queue('rep-2');
      final t = FakeTransport();
      final up = makeUploader(t, FakeSource(), outbox: outbox, feedback: true)..start();
      t.receive(stateMsg(state, interval: 0));
      await until(() => t.ofType('feedback-begin').isNotEmpty);
      final begin = t.ofType('feedback-begin').single;
      expect(begin['pngBytes'], 0);
      expect(begin['chunks'], 0);
      await Future<void>.delayed(const Duration(milliseconds: 100));
      expect(t.ofType('feedback-chunk'), isEmpty);
      expect(inOutbox('rep-2'), isTrue);
      t.receive({...contract['feedback-ack-ok']!, 'id': 'rep-2'});
      await until(() => !inOutbox('rep-2'));
      up.dispose();
    });

    test('rejected keeps the report; other PCs reports are not sent here', () async {
      await queue('rep-3');
      await queue('rep-4', pc: 'pc-2');
      final t = FakeTransport();
      final up = makeUploader(t, FakeSource(), outbox: outbox, feedback: true)..start();
      t.receive(stateMsg(state, interval: 0));
      await until(() => t.ofType('feedback-begin').isNotEmpty);
      t.receive({...contract['feedback-ack-ok']!, 'id': 'rep-3', 'status': 'rejected'});
      await Future<void>.delayed(const Duration(milliseconds: 200));
      expect(t.ofType('feedback-begin').map((m) => m['id']), ['rep-3']);
      expect(inOutbox('rep-3'), isTrue);
      expect(inOutbox('rep-4'), isTrue);
      up.dispose();
    });

    test('a queued report goes first; history follows its ack', () async {
      await queue('rep-5');
      final src = FakeSource()..addMany(3, pc: 'pc-1');
      final t = FakeTransport();
      final up = makeUploader(t, src, outbox: outbox, feedback: true)..start();
      t.receive(stateMsg(state, interval: 0));
      await until(() => t.ofType('feedback-begin').isNotEmpty);
      expect(t.ofType('logs-batch'), isEmpty, reason: 'feedback is acknowledged first');
      t.receive({...contract['feedback-ack-ok']!, 'id': 'rep-5'});
      await until(() => t.ofType('logs-batch').isNotEmpty);
      up.dispose();
    });

    test('feedback respects the priority gate too', () async {
      await queue('rep-6');
      final t = FakeTransport()..bufferedBytes = 1 << 20;
      final up = makeUploader(t, FakeSource(), outbox: outbox, feedback: true)..start();
      t.receive(stateMsg(state, interval: 0));
      await Future<void>.delayed(const Duration(milliseconds: 300));
      expect(t.sent, isEmpty);
      t.bufferedBytes = 0;
      await until(() => t.ofType('feedback-begin').isNotEmpty);
      up.dispose();
    });
  });

  group('real logger and store', () {
    late Directory tmp;

    setUp(() => tmp = Directory.systemTemp.createTempSync('bp-upload-store-'));

    tearDown(() {
      try {
        tmp.deleteSync(recursive: true);
      } catch (_) {}
    });

    test('cursors are fixed width and increase byte-wise past ten lines in one millisecond',
        () async {
      final fixed = DateTime.utc(2026, 11, 2, 13, 45, 12, 345);
      final logger = AppLogger(
        src: 'app-win',
        store: LogStore(tmp, src: 'app-win'),
        clock: () => fixed,
      );
      for (var i = 0; i < 120; i++) {
        logger.log(LogLevel.info, 'T', 'm$i', pc: 'pc-1');
      }
      await logger.flush();
      final cursors = <String>[];
      String? after;
      while (true) {
        final page = await logger.linesFor('pc-1', afterCursor: after, limit: 7);
        if (page.lines.isEmpty) break;
        cursors.add(page.cursor!);
        after = page.cursor;
      }
      expect(cursors.length, greaterThan(10));
      for (final c in cursors) {
        expect(c.length, cursors.first.length);
      }
      final sorted = [...cursors]..sort();
      expect(cursors, sorted);
      expect(cursors.last, '2026-11-02T13:45:12.345Z#000120');
      // The older unpadded form is still read back.
      final page = await logger.linesFor('pc-1', afterCursor: '2026-11-02T13:45:12.345Z#118');
      expect(page.lines.length, 2);
    });

    test('uploads only its own stream, history then live, and resumes without repeats',
        () async {
      var now = DateTime.utc(2026, 11, 2, 13, 0, 0);
      final logger = AppLogger(
        src: 'app-win',
        store: LogStore(tmp, src: 'app-win'),
        clock: () => now,
      );
      // The Kotlin core writes its own file into the same tree.
      final core = LogStore(tmp, src: 'app-android-core');
      await core.append('pc-1', {
        '2026-11-02': [
          jsonEncode({'ts': '2026-11-02T13:00:00.500Z', 'src': 'app-android-core', 'msg': 'k'}),
        ],
      });
      for (var i = 0; i < 5; i++) {
        now = now.add(const Duration(milliseconds: 100));
        logger.log(LogLevel.info, 'T', 'pc line $i', pc: 'pc-1');
      }
      now = now.add(const Duration(milliseconds: 100));
      logger.log(LogLevel.info, 'T', 'misc line');
      now = now.add(const Duration(milliseconds: 100));
      logger.log(LogLevel.info, 'T', 'other pc', pc: 'pc-2');
      await logger.flush();

      final t = FakeTransport();
      final up = ClientLogUploader(
        transport: t,
        appLog: logger,
        outbox: Outbox.forRoot(tmp),
        stream: 'app-win',
        pcId: 'pc-1',
        sendFeedback: false,
      )..start();
      t.receive(stateMsg(state, interval: 0));
      Future<void> until(bool Function() c) async {
        final end = DateTime.now().add(const Duration(seconds: 5));
        while (!c() && DateTime.now().isBefore(end)) {
          await Future<void>.delayed(const Duration(milliseconds: 5));
        }
        expect(c(), isTrue, reason: 'timed out');
      }

      await until(() => t.ofType('logs-batch').isNotEmpty);
      final b1 = t.ofType('logs-batch').single;
      final msgs1 = [
        for (final l in b1['lines'] as List) (jsonDecode(l as String) as Map)['msg'],
      ];
      expect(msgs1, ['pc line 0', 'pc line 1', 'pc line 2', 'pc line 3', 'pc line 4', 'misc line']);
      t.receive(ack(okAck, 1, cursor: b1['to']! as String));
      now = now.add(const Duration(milliseconds: 100));
      logger.log(LogLevel.info, 'T', 'live line', pc: 'pc-1');
      await until(() => t.ofType('logs-batch').length == 2);
      final b2 = t.ofType('logs-batch')[1];
      expect(b2['live'], true);
      expect(b2['from'], b1['to']);
      expect([
        for (final l in b2['lines'] as List) (jsonDecode(l as String) as Map)['msg'],
      ], ['live line']);
      up.dispose();

      // A new connection: the host's cursor says everything up to the live line is stored.
      final t2 = FakeTransport();
      final up2 = ClientLogUploader(
        transport: t2,
        appLog: logger,
        outbox: Outbox.forRoot(tmp),
        stream: 'app-win',
        pcId: 'pc-1',
        sendFeedback: false,
      )..start();
      t2.receive(stateMsg(state, cursors: {'app-win': b2['to']! as String}, interval: 0));
      await Future<void>.delayed(const Duration(milliseconds: 200));
      expect(t2.sent, isEmpty);
      now = now.add(const Duration(milliseconds: 100));
      logger.log(LogLevel.info, 'T', 'after reconnect', pc: 'pc-1');
      await until(() => t2.ofType('logs-batch').isNotEmpty);
      expect((t2.ofType('logs-batch').single['lines'] as List).length, 1);
      up2.dispose();
    });
  });
}
