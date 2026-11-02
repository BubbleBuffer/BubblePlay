import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:ui';

import 'package:bubbleplay_app/src/logging/app_log.dart';
import 'package:bubbleplay_app/src/logging/error_hooks.dart';
import 'package:bubbleplay_app/src/logging/log_store.dart';
import 'package:bubbleplay_app/src/logging/redact.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_test/flutter_test.dart';

final _tsFormat = RegExp(r'^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$');

late Directory _root;

/// A fixed clock, moved by [advance].
class _Clock {
  _Clock(this.now);

  DateTime now;

  DateTime call() => now;
}

AppLogger _logger({
  String src = 'app-win',
  _Clock? clock,
  int queueLimit = 2000,
  int perPcCap = 5 * 1024 * 1024,
  int totalCap = 20 * 1024 * 1024,
}) {
  final c = clock ?? _Clock(DateTime.utc(2026, 11, 2, 13, 45, 12, 345));
  return AppLogger(
    src: src,
    store: LogStore(_root, src: src, clock: c.call, perPcCap: perPcCap, totalCap: totalCap),
    device: 'DEV-1',
    clock: c.call,
    queueLimit: queueLimit,
  );
}

List<File> _files(Directory dir) => [
      if (dir.existsSync())
        for (final e in dir.listSync(recursive: true))
          if (e is File) e,
    ];

String _allText() => _files(_root).map((f) => f.readAsStringSync()).join('\n');

List<Map<String, Object?>> _lines(File f) => [
      for (final l in const LineSplitter().convert(f.readAsStringSync()))
        (jsonDecode(l) as Map).cast<String, Object?>(),
    ];

void main() {
  setUp(() => _root = Directory.systemTemp.createTempSync('bp-log-test'));
  tearDown(() => _root.deleteSync(recursive: true));

  group('line format (spec section 1)', () {
    test('writes one JSON object per line with every field', () async {
      final log = _logger();
      log.setContext(pc: 'PC-A', session: 'S1');
      log.log(LogLevel.info, 'Gate', 'hello', fields: {'rtt_ms': 40});
      await log.flush();
      final file = File('${_root.path}/pc/PC-A/app-win-2026-11-02.jsonl');
      expect(file.existsSync(), isTrue);
      final line = _lines(file).single;
      expect(line['ts'], '2026-11-02T13:45:12.345Z');
      expect(line['ts'], matches(_tsFormat));
      expect(line['level'], 'info');
      expect(line['src'], 'app-win');
      expect(line['device'], 'DEV-1');
      expect(line['pc'], 'PC-A');
      expect(line['session'], 'S1');
      expect(line['target'], 'Gate');
      expect(line['msg'], 'hello');
      expect(line['fields'], {'rtt_ms': 40});
      expect(file.readAsStringSync().endsWith('\n'), isTrue);
    });

    test('lines without a PC go to misc; an unsafe PC id goes there too', () async {
      final log = _logger();
      log.log(LogLevel.info, 'T', 'no pc');
      log.log(LogLevel.info, 'T', 'bad pc', pc: '../../etc');
      await log.flush();
      final misc = File('${_root.path}/misc/app-win-2026-11-02.jsonl');
      final lines = _lines(misc);
      expect(lines.map((l) => l['msg']), ['no pc', 'bad pc']);
      expect(lines.every((l) => l['pc'] == null), isTrue);
      expect(Directory('${_root.path}/pc').existsSync(), isFalse);
    });

    test('the default level drops debug lines', () async {
      final log = _logger();
      log.log(LogLevel.debug, 'T', 'quiet');
      log.log(LogLevel.warn, 'T', 'loud');
      await log.flush();
      expect(_allText(), isNot(contains('quiet')));
      expect(_allText(), contains('loud'));
    });

    test('error and stack go into fields', () async {
      final log = _logger();
      log.log(LogLevel.error, 'T', 'failed',
          error: StateError('nope'), stack: StackTrace.fromString('#0 here'));
      await log.flush();
      final line = _lines(_files(_root).single).single;
      final fields = line['fields']! as Map;
      expect(fields['error'], contains('nope'));
      expect(fields['error_type'], 'StateError');
      expect(fields['stack'], '#0 here');
    });

    test('a line never exceeds 8 KiB and stays valid JSON', () async {
      final log = _logger();
      log.log(LogLevel.info, 'T', 'é' * 30000, fields: {
        for (var i = 0; i < 12; i++) 'f$i': 'ü' * 5000,
        'stack': 'x y ' * 5000,
      });
      log.log(LogLevel.info, 'T', 'm ' * 15000);
      await log.flush();
      final raw = const LineSplitter().convert(_allText());
      expect(raw.length, 2);
      for (final l in raw) {
        expect(utf8.encode(l).length + 1, lessThanOrEqualTo(maxLineBytes));
        expect((jsonDecode(l) as Map)['msg'], isA<String>());
      }
      expect(((jsonDecode(raw.last) as Map)['msg'] as String).endsWith('...[truncated]'), isTrue);
    });

    test('a full queue drops the oldest and reports the count', () async {
      final log = _logger(queueLimit: 5);
      for (var i = 0; i < 20; i++) {
        log.log(LogLevel.info, 'T', 'line $i');
      }
      expect(log.droppedPending, 15);
      await log.flush();
      final lines = const LineSplitter().convert(_allText()).map((l) => jsonDecode(l) as Map);
      final note = lines.firstWhere((l) => l['target'] == 'AppLog');
      expect((note['fields']! as Map)['dropped'], 15);
      expect(lines.any((l) => l['msg'] == 'line 19'), isTrue);
      expect(lines.any((l) => l['msg'] == 'line 0'), isFalse);
    });

    test('without a store nothing is written and nothing throws', () {
      final log = AppLogger(src: 'app-win');
      log.log(LogLevel.error, 'T', 'in memory only');
      expect(log.recentLines.single, contains('in memory only'));
      expect(log.recentErrors.length, 1);
    });
  });

  group('secrets never reach the file', () {
    test('registered literals, field names, bearer, urls, blobs', () async {
      final log = _logger();
      log.registerSecret('sup3r-s3cret-literal');
      log.log(
        LogLevel.error,
        'T',
        'failed with sup3r-s3cret-literal at https://r.example/p?token=tok123&code=999 '
        'Bearer abc123 password=hunter2 ${'Q' * 50}',
        error: Exception('inner sup3r-s3cret-literal'),
        stack: StackTrace.fromString('frame sup3r-s3cret-literal'),
        fields: {
          'pairing_code': '123456',
          'passphrase': 'correct horse',
          'note': 'has sup3r-s3cret-literal inside',
          'nested': {'api_key': 'KEY-VALUE-1', 'list': ['sup3r-s3cret-literal']},
        },
      );
      await log.flush();
      final text = _allText();
      for (final secret in [
        'sup3r-s3cret-literal',
        'tok123',
        '999',
        'abc123',
        'hunter2',
        '123456',
        'correct horse',
        'KEY-VALUE-1',
        'Q' * 50,
      ]) {
        expect(text, isNot(contains(secret)), reason: secret);
      }
      expect(text, contains(redacted));
    });
  });

  group('store caps (spec section 3)', () {
    File put(String rel, int bytes, {String ts = '2026-10-30T10:00:00.000Z'}) {
      final f = File('${_root.path}/$rel')..createSync(recursive: true);
      final line = '${jsonEncode({'ts': ts, 'level': 'info', 'msg': 'x'})}\n';
      final sb = StringBuffer();
      while (sb.length < bytes) {
        sb.write(line);
      }
      f.writeAsStringSync(sb.toString());
      return f;
    }

    test('the per-PC cap deletes the oldest day files first, any process', () async {
      final clock = _Clock(DateTime.utc(2026, 11, 2, 12));
      final store = LogStore(_root,
          src: 'app-win', clock: clock.call, perPcCap: 3000, totalCap: 1 << 30);
      final a = put('pc/PC-A/app-win-2026-10-29.jsonl', 1500);
      final b = put('pc/PC-A/app-android-core-2026-10-30.jsonl', 1500);
      final c = put('pc/PC-A/app-win-2026-10-31.jsonl', 1500);
      final today = put('pc/PC-A/app-win-2026-11-02.jsonl', 600);
      final other = put('pc/PC-B/app-win-2026-10-29.jsonl', 1500);
      await store.enforceCaps();
      expect(a.existsSync(), isFalse);
      expect(b.existsSync(), isFalse);
      expect(c.existsSync(), isTrue);
      expect(today.existsSync(), isTrue);
      expect(other.existsSync(), isTrue, reason: 'another PC is under its own cap');
    });

    test('over the cap with only today left trims this process\'s file to its newest lines',
        () async {
      final clock = _Clock(DateTime.utc(2026, 11, 2, 12));
      final store = LogStore(_root,
          src: 'app-win', clock: clock.call, perPcCap: 2000, totalCap: 1 << 30);
      final f = put('pc/PC-A/app-win-2026-11-02.jsonl', 6000, ts: '2026-11-02T11:00:00.000Z');
      await store.enforceCaps();
      expect(f.existsSync(), isTrue);
      expect(f.lengthSync(), lessThanOrEqualTo(2000));
      for (final l in const LineSplitter().convert(f.readAsStringSync())) {
        expect(jsonDecode(l), isA<Map>(), reason: 'trimmed on a line start');
      }
    });

    test('the total cap spans PCs and misc, oldest day first', () async {
      final clock = _Clock(DateTime.utc(2026, 11, 2, 12));
      final store = LogStore(_root,
          src: 'app-win', clock: clock.call, perPcCap: 1 << 30, totalCap: 3300);
      final old = put('misc/app-win-2026-10-28.jsonl', 1500);
      final mid = put('pc/PC-A/app-win-2026-10-30.jsonl', 1500);
      final fresh = put('pc/PC-B/app-win-2026-10-31.jsonl', 1500);
      await store.enforceCaps();
      expect(old.existsSync(), isFalse);
      expect(mid.existsSync(), isTrue);
      expect(fresh.existsSync(), isTrue);
    });

    test('files older than 30 days go, and empty PC directories with them', () async {
      final clock = _Clock(DateTime.utc(2026, 11, 2, 12));
      final store = LogStore(_root, src: 'app-win', clock: clock.call);
      put('pc/PC-OLD/app-win-2026-09-01.jsonl', 100);
      final keep = put('pc/PC-NEW/app-win-2026-10-10.jsonl', 100);
      await store.enforceCaps();
      expect(Directory('${_root.path}/pc/PC-OLD').existsSync(), isFalse);
      expect(keep.existsSync(), isTrue);
    });

    test('writing enough through the logger triggers the caps', () async {
      final log = _logger(perPcCap: 100 * 1024);
      final store = log.store!;
      put('pc/PC-A/app-win-2026-10-30.jsonl', 90 * 1024);
      for (var i = 0; i < 400; i++) {
        log.log(LogLevel.info, 'T', 'padding ${'word ' * 140}', pc: 'PC-A');
      }
      await log.flush();
      expect(store.wantsEnforce, isFalse, reason: 'enforced after the batch');
      expect(File('${_root.path}/pc/PC-A/app-win-2026-10-30.jsonl').existsSync(), isFalse);
    });
  });

  group('reading back', () {
    test('merges the files of every process by ts', () async {
      File('${_root.path}/pc/PC-A/app-android-core-2026-11-02.jsonl')
        ..createSync(recursive: true)
        ..writeAsStringSync(
          '${jsonEncode({'ts': '2026-11-02T13:45:12.100Z', 'level': 'info', 'msg': 'core early'})}\n'
          '${jsonEncode({'ts': '2026-11-02T13:45:12.900Z', 'level': 'error', 'msg': 'core late'})}\n',
        );
      final log = _logger();
      log.log(LogLevel.info, 'T', 'dart middle', pc: 'PC-A');
      await log.flush();
      AppLog.install(log);
      final page = await AppLog.linesFor('PC-A');
      expect(
        page.lines.map((l) => (jsonDecode(l) as Map)['msg']),
        ['core early', 'dart middle', 'core late'],
      );
      final report = await log.reportLogs(pc: 'PC-A');
      expect(report.logs.length, 3);
      expect(report.errors.single['msg'], 'core late');
    });

    test('linesFor pages with a cursor: nothing twice, nothing skipped, even on equal ts',
        () async {
      final lines = [
        for (var i = 0; i < 7; i++)
          jsonEncode({
            'ts': i < 4 ? '2026-11-02T10:00:00.000Z' : '2026-11-02T10:00:0$i.000Z',
            'level': 'info',
            'msg': 'm$i',
          }),
      ];
      File('${_root.path}/pc/PC-A/app-win-2026-11-02.jsonl')
        ..createSync(recursive: true)
        ..writeAsStringSync('${lines.join('\n')}\n');
      final log = _logger();
      AppLog.install(log);
      final got = <String>[];
      String? cursor;
      for (var round = 0; round < 10; round++) {
        final page = await AppLog.linesFor('PC-A', afterCursor: cursor, limit: 2);
        got.addAll(page.lines.map((l) => (jsonDecode(l) as Map)['msg'] as String));
        cursor = page.cursor;
        if (!page.more) break;
      }
      expect(got, ['m0', 'm1', 'm2', 'm3', 'm4', 'm5', 'm6']);
      // At the end the cursor yields nothing and stays put.
      final end = await AppLog.linesFor('PC-A', afterCursor: cursor);
      expect(end.lines, isEmpty);
      expect(end.cursor, cursor);
      // New lines after the cursor arrive; the old ones do not repeat.
      log.log(LogLevel.info, 'T', 'm7', pc: 'PC-A');
      final more = await AppLog.linesFor('PC-A', afterCursor: cursor);
      expect(more.lines.map((l) => (jsonDecode(l) as Map)['msg']), ['m7']);
    });

    test('misc lines have their own cursor and unsafe ids return nothing', () async {
      final log = _logger();
      AppLog.install(log);
      log.log(LogLevel.info, 'T', 'misc one');
      final page = await AppLog.miscLines();
      expect(page.lines.length, 1);
      expect((await AppLog.miscLines(afterCursor: page.cursor)).lines, isEmpty);
      expect((await AppLog.linesFor('../x')).lines, isEmpty);
    });
  });

  group('error hooks', () {
    late FlutterExceptionHandler? savedFlutter;
    late ErrorCallback? savedPlatform;

    setUp(() {
      savedFlutter = FlutterError.onError;
      savedPlatform = PlatformDispatcher.instance.onError;
      FlutterError.onError = (_) {};
      PlatformDispatcher.instance.onError = (e, s) => true;
    });
    tearDown(() {
      FlutterError.onError = savedFlutter;
      PlatformDispatcher.instance.onError = savedPlatform;
    });

    test('FlutterError.onError writes an uncaught line and chains the old handler', () async {
      final log = _logger();
      var chained = 0;
      FlutterError.onError = (_) => chained++;
      installErrorHooks(log: log);
      FlutterError.reportError(FlutterErrorDetails(
        exception: StateError('widget exploded'),
        stack: StackTrace.fromString('#0 build'),
      ));
      await log.flush();
      final line = _lines(_files(_root).single).single;
      expect(line['level'], 'error');
      expect(line['target'], 'uncaught');
      final fields = line['fields']! as Map;
      expect(fields['zone'], 'flutter');
      expect(fields['error'], contains('widget exploded'));
      expect(fields['stack'], '#0 build');
      expect(chained, 1);
      expect(log.recentErrors.length, 1);
    });

    test('PlatformDispatcher.onError writes an uncaught line', () async {
      final log = _logger();
      installErrorHooks(log: log);
      final handled = PlatformDispatcher.instance.onError!(
        StateError('async broke'),
        StackTrace.fromString('#0 later'),
      );
      expect(handled, isTrue, reason: 'the previous handler still decides');
      await log.flush();
      final line = _lines(_files(_root).single).single;
      expect((line['fields']! as Map)['zone'], 'platform');
      expect((line['fields']! as Map)['error'], contains('async broke'));
    });

    test('installing twice does not log an error twice', () async {
      final log = _logger();
      installErrorHooks(log: log);
      installErrorHooks(log: log);
      FlutterError.reportError(FlutterErrorDetails(exception: StateError('once')));
      await log.flush();
      expect(const LineSplitter().convert(_allText()).length, 1);
    });

    test('runGuarded logs errors that escape every other handler', () async {
      final log = _logger();
      runGuarded(() {
        Timer.run(() => throw StateError('zone boom'));
      }, log: log);
      await Future<void>.delayed(const Duration(milliseconds: 50));
      await log.flush();
      final line = _lines(_files(_root).single).single;
      expect((line['fields']! as Map)['zone'], 'zone');
      expect((line['fields']! as Map)['error'], contains('zone boom'));
    });

    test('the ring keeps the last 20 errors', () {
      final log = AppLogger(src: 'app-win');
      for (var i = 0; i < 30; i++) {
        log.log(LogLevel.error, 'T', 'e$i');
      }
      expect(log.recentErrors.length, ringErrors);
      expect(log.recentErrors.last, contains('e29'));
      expect(log.recentErrors.first, contains('e10'));
    });
  });
}
