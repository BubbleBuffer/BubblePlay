import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:bubbleplay_app/src/feedback/outbox.dart';
import 'package:bubbleplay_app/src/feedback/report.dart';
import 'package:bubbleplay_app/src/logging/upload/ctl_bridge.dart';
import 'package:bubbleplay_app/src/logging/upload/ctl_bridge_factory.dart';
import 'package:bubbleplay_app/src/logging/upload/upload_coordinator.dart';
// fake_async comes with flutter_test; the pubspec is out of this lane's scope.
// ignore: depend_on_referenced_packages
import 'package:fake_async/fake_async.dart';
import 'package:flutter_test/flutter_test.dart';

import 'client_log_uploader_test.dart' show FakeSource, loadContract;

typedef Json = Map<String, dynamic>;

const stream = 'app-win';
const caps = {clientLogsCapability, 'control-aead-v1'};

class FakeBridge implements CtlBridge {
  final StreamController<CtlEvent> _events = StreamController<CtlEvent>.broadcast(sync: true);
  final List<Json> sent = [];

  /// Decides the result of each send; null: succeeds.
  Future<bool> Function(Json message)? onSend;

  @override
  int bufferedBytes = 0;

  @override
  Duration sinceLastInput = const Duration(seconds: 10);

  @override
  Stream<CtlEvent> get events => _events.stream;

  @override
  Future<bool> send(Json message) async {
    sent.add((jsonDecode(jsonEncode(message)) as Map).cast<String, dynamic>());
    final f = onSend;
    return f == null ? true : f(message);
  }

  void open({String pc = 'pc-1', Set<String> peerCaps = caps}) => _events.add(CtlOpened(pc, peerCaps));
  void message(Json m) => _events.add(CtlMessage(m));
  void close() => _events.add(const CtlClosed());
  void error() => _events.addError(StateError('bridge broke'));

  List<Json> ofType(String type) => [
        for (final m in sent)
          if (m['type'] == type) m,
      ];
}

Json copyOf(Map<String, Object?> m) => (jsonDecode(jsonEncode(m)) as Map).cast<String, dynamic>();

void main() {
  final fixture = {for (final e in loadContract().entries) e.key: copyOf(e.value)};

  Json state() {
    final m = copyOf(fixture['logs-state']!);
    m['cursors'] = <String, Object?>{};
    (m['limits'] as Map)['minBatchIntervalMs'] = 0;
    return m;
  }

  Json logsAck(int id, String cursor) => copyOf(fixture['logs-ack-ok']!)
    ..['stream'] = stream
    ..['id'] = id
    ..['cursor'] = cursor;

  Outbox noOutbox() => Outbox(Directory('${Directory.systemTemp.path}/bp-no-outbox-dir'));

  UploadCoordinator make(FakeBridge b, FakeSource src, {Outbox? outbox}) => UploadCoordinator(
        bridge: b,
        appLog: src,
        outbox: outbox ?? noOutbox(),
        stream: stream,
        feedbackPollInterval: Duration.zero,
        feedbackRetryDelay: const Duration(milliseconds: 40),
      )..start();

  test('the platform factory has no bridge until a platform lane provides one', () {
    expect(createPlatformCtlBridge(), isNull);
  });

  group('logs (fake async)', () {
    test('opened builds an uploader, closed disposes it, a new connection starts over', () {
      fakeAsync((async) {
        final b = FakeBridge();
        final src = FakeSource()..addMany(3, pc: 'pc-1');
        final c = make(b, src);
        expect(c.isConnected, isFalse);
        b.open();
        final first = c.uploader!;
        b.message(state());
        async.elapse(const Duration(milliseconds: 50));
        expect(b.ofType('logs-batch').single['id'], 1);
        b.close();
        expect(c.isConnected, isFalse);
        b.sent.clear();
        async.elapse(const Duration(seconds: 5));
        expect(b.sent, isEmpty, reason: 'a closed connection sends nothing');
        b.open();
        expect(c.uploader, isNot(same(first)));
        b.message(state());
        async.elapse(const Duration(milliseconds: 50));
        final again = b.ofType('logs-batch').single;
        expect(again['id'], 1, reason: 'message ids restart per connection');
        expect(again['from'], '0');
        // Opened without a closed in between replaces the uploader too.
        final second = c.uploader!;
        b.open(pc: 'pc-2');
        expect(c.uploader, isNot(same(second)));
        c.dispose();
      });
    });

    test('a PC without the capability gets no uploader', () {
      fakeAsync((async) {
        final b = FakeBridge();
        final c = make(b, FakeSource()..addMany(2, pc: 'pc-1'));
        b.open(peerCaps: {'control-aead-v1'});
        b.message(state());
        async.elapse(const Duration(seconds: 2));
        expect(c.isConnected, isFalse);
        expect(b.sent, isEmpty);
        c.dispose();
      });
    });

    test('contract round trip: logs-state, logs-batch, logs-ack, cursor advances', () {
      fakeAsync((async) {
        final b = FakeBridge();
        final src = FakeSource()..addMany(3, pc: 'pc-1');
        final c = make(b, src);
        b.open();
        // Nothing before the host's logs-state.
        async.elapse(const Duration(seconds: 2));
        expect(b.sent, isEmpty);
        b.message(state());
        async.elapse(const Duration(milliseconds: 50));
        final batch = b.ofType('logs-batch').single;
        expect(batch.keys.toSet(), fixture['logs-batch']!.keys.toSet());
        expect(batch['stream'], stream);
        expect((batch['lines'] as List).length, 3);
        final to = batch['to'] as String;
        expect(c.uploader!.cursor, isNull);
        b.message(logsAck(1, to));
        async.elapse(const Duration(milliseconds: 50));
        expect(c.uploader!.cursor, to);
        // A line written later goes out as the next, live batch from the new cursor.
        src.add(FakeSource.tsOf(10), pc: 'pc-1', msg: 'later');
        async.elapse(const Duration(seconds: 2));
        final next = b.ofType('logs-batch').last;
        expect(next['id'], 2);
        expect(next['from'], to);
        expect(next['live'], isTrue);
        c.dispose();
      });
    });

    test('a refused send (false) is retried later with the same batch', () {
      fakeAsync((async) {
        final b = FakeBridge();
        var refuse = true;
        b.onSend = (m) async => !refuse;
        final src = FakeSource()..addMany(3, pc: 'pc-1');
        final c = make(b, src);
        b.open();
        b.message(state());
        async.elapse(const Duration(milliseconds: 300));
        expect(b.ofType('logs-batch').length, 1);
        refuse = false;
        async.elapse(const Duration(seconds: 2));
        final batches = b.ofType('logs-batch');
        expect(batches.length, 2);
        expect(batches[1], batches[0], reason: 'the same batch, same id');
        expect(c.isConnected, isTrue);
        c.dispose();
      });
    });

    test('bridge errors never escape: failing send, error events, odd messages', () {
      fakeAsync((async) {
        final b = FakeBridge();
        b.onSend = (m) async => throw StateError('send blew up');
        final src = FakeSource()..addMany(3, pc: 'pc-1');
        final c = make(b, src);
        b.error();
        b.open();
        b.error();
        b.message(state());
        b.message({'type': 'logs-ack', 'v': 1}); // incomplete: ignored
        b.message({'garbage': 1});
        async.elapse(const Duration(seconds: 5));
        expect(b.ofType('logs-batch'), isNotEmpty);
        expect(c.isConnected, isTrue);
        b.close();
        c.dispose();
        expect(async.flushMicrotasks, returnsNormally);
      });
    });

    test('dispose during a pending batch: nothing more is sent, a late result is dropped', () {
      fakeAsync((async) {
        final b = FakeBridge();
        final gate = Completer<bool>();
        b.onSend = (m) => gate.future;
        final src = FakeSource()..addMany(250, pc: 'pc-1');
        final c = make(b, src);
        b.open();
        b.message(state());
        async.elapse(const Duration(milliseconds: 50));
        expect(b.ofType('logs-batch').length, 1);
        c.dispose();
        gate.complete(false); // would have asked for a retry
        async.elapse(const Duration(seconds: 10));
        expect(b.ofType('logs-batch').length, 1);
        // Events after dispose are ignored.
        b.open();
        b.message(state());
        async.elapse(const Duration(seconds: 2));
        expect(b.ofType('logs-batch').length, 1);
        expect(c.isConnected, isFalse);
        c.dispose(); // twice is fine
      });
    });

    test('an ack of the old connection does not count in the next one', () {
      fakeAsync((async) {
        final b = FakeBridge();
        final src = FakeSource()..addMany(3, pc: 'pc-1');
        final c = make(b, src);
        b.open();
        b.message(state());
        async.elapse(const Duration(milliseconds: 50));
        final to = b.ofType('logs-batch').single['to'] as String;
        b.close();
        b.open();
        b.message(logsAck(1, to)); // before the new logs-state: nothing is in flight yet
        async.elapse(const Duration(milliseconds: 50));
        expect(c.uploader!.cursor, isNull);
        c.dispose();
      });
    });
  });

  group('feedback (real files)', () {
    late Directory tmp;
    late Outbox outbox;

    setUp(() {
      tmp = Directory.systemTemp.createTempSync('bp-coord-');
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

    Future<void> queue(String id, {Uint8List? png}) {
      final created = DateTime.utc(2026, 11, 2, 13, 50);
      return outbox.add(
        FeedbackReport(id: id, created: created, json: {
          'version': 1,
          'id': id,
          'created': '2026-11-02T13:50:00.000Z',
          'text': 'Stream froze',
          'kind': 'bug',
          'pc': 'pc-1',
          'logs': <Object?>[],
          'errors': <Object?>[],
          'screenshot': png == null ? null : Outbox.screenshotName(id, created),
        }),
        screenshot: png,
      );
    }

    bool inOutbox(String id) =>
        Directory('${tmp.path}/outbox').existsSync() &&
        Directory('${tmp.path}/outbox').listSync().any((e) => e.path.contains(id));

    Json feedbackOk(String id) => copyOf(fixture['feedback-ack-ok']!)..['id'] = id;

    test('a report with a PNG: begin and chunks, removed and announced after the ok', () async {
      final png = Uint8List.fromList(List.generate(70000, (i) => i % 251));
      await queue('rep-1', png: png);
      final b = FakeBridge();
      final c = make(b, FakeSource(), outbox: outbox);
      final announced = <String>[];
      c.sentReports.listen(announced.add);
      b.open();
      b.message(state());
      await until(() => b.ofType('feedback-chunk').length == 3);
      final begin = b.ofType('feedback-begin').single;
      expect(begin.keys.toSet(), fixture['feedback-begin']!.keys.toSet());
      expect(begin['pngBytes'], 70000);
      final bytes = BytesBuilder();
      for (final ch in b.ofType('feedback-chunk')) {
        bytes.add(base64Decode(ch['data'] as String));
      }
      expect(bytes.toBytes(), png);
      await Future<void>.delayed(const Duration(milliseconds: 80));
      expect(inOutbox('rep-1'), isTrue);
      expect(announced, isEmpty);
      b.message(feedbackOk('rep-1'));
      await until(() => !inOutbox('rep-1'));
      await until(() => announced.isNotEmpty);
      expect(announced, ['rep-1']);
      c.dispose();
    });

    test('a report without a PNG: no chunks; saved while connected goes out at once', () async {
      final b = FakeBridge();
      final c = make(b, FakeSource(), outbox: outbox);
      b.open();
      b.message(state());
      await Future<void>.delayed(const Duration(milliseconds: 100));
      expect(b.ofType('feedback-begin'), isEmpty);
      // Saved now (the poll is off): only checkFeedback brings it out.
      await queue('rep-2');
      c.checkFeedback();
      await until(() => b.ofType('feedback-begin').isNotEmpty);
      final begin = b.ofType('feedback-begin').single;
      expect(begin['id'], 'rep-2');
      expect(begin['pngBytes'], 0);
      expect(begin['chunks'], 0);
      await Future<void>.delayed(const Duration(milliseconds: 80));
      expect(b.ofType('feedback-chunk'), isEmpty);
      b.message(feedbackOk('rep-2'));
      await until(() => !inOutbox('rep-2'));
      c.dispose();
    });

    test('a refused feedback message starts the report over; the outbox keeps it', () async {
      final png = Uint8List.fromList(List.generate(70000, (i) => i % 251));
      await queue('rep-3', png: png);
      final b = FakeBridge();
      var refuse = true;
      b.onSend = (m) async => !(refuse && m['type'] == 'feedback-chunk' && m['i'] == 1);
      final c = make(b, FakeSource(), outbox: outbox);
      b.open();
      b.message(state());
      await until(() => b.ofType('feedback-begin').length == 2);
      // The first round stopped at the refused chunk 1: chunk 2 never went out.
      final begins = [
        for (var i = 0; i < b.sent.length; i++)
          if (b.sent[i]['type'] == 'feedback-begin') i,
      ];
      final firstRound = b.sent.sublist(0, begins[1]);
      expect(
        [for (final m in firstRound) if (m['type'] == 'feedback-chunk') m['i']],
        [0, 1],
      );
      refuse = false;
      await until(() => b.ofType('feedback-chunk').where((m) => m['i'] == 2).isNotEmpty);
      expect(inOutbox('rep-3'), isTrue);
      b.message(feedbackOk('rep-3'));
      await until(() => !inOutbox('rep-3'));
      c.dispose();
    });
  });
}
