import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:bubbleplay_app/src/feedback/outbox.dart';
import 'package:bubbleplay_app/src/feedback/report.dart';
import 'package:bubbleplay_app/src/logging/app_log.dart';
import 'package:flutter_test/flutter_test.dart';

late Directory _root;

FeedbackReport _report(AppLogger log, {String text = 'it broke', DateTime? now, String? shot}) =>
    buildReport(
      log: log,
      now: now ?? DateTime.utc(2026, 11, 2, 13, 45, 12, 345),
      text: text,
      kind: FeedbackKind.bug,
      app: const FeedbackApp(version: '0.1.0', build: '1', commit: ''),
      platform: 'windows',
      device: const FeedbackDevice(id: 'DEV-1', name: 'laptop', model: 'Windows 11'),
      pc: 'PC-A',
      session: null,
      route: 'home',
      locale: 'en-US',
      logs: const ReportLogs([], []),
      screenshotFile: shot,
    );

void main() {
  setUp(() => _root = Directory.systemTemp.createTempSync('bp-outbox-test'));
  tearDown(() => _root.deleteSync(recursive: true));

  test('add stores one JSON, plus the PNG when given; pending lists oldest first', () async {
    final log = AppLogger(src: 'app-win');
    final outbox = Outbox.forRoot(_root);
    final early = _report(log, now: DateTime.utc(2026, 11, 2, 9));
    final late = _report(log, text: 'second', shot: 'x.png');
    await outbox.add(late, screenshot: Uint8List.fromList([1, 2, 3]));
    await outbox.add(early);
    final pending = await outbox.pending();
    expect(pending.map((i) => i.id), [early.id, late.id]);
    expect(pending.first.png, isNull);
    expect(pending.last.png, isNotNull);
    expect(pending.last.png!.readAsBytesSync(), [1, 2, 3]);
    expect(pending.last.report['text'], 'second');
    // Names: <UTC stamp>-<id>.json and .png, nothing half-written left over.
    final names = _root.listSync(recursive: true).map((e) => e.uri.pathSegments.last).toList();
    expect(names, contains('20261102T134512345Z-${late.id}.json'));
    expect(names, contains('20261102T134512345Z-${late.id}.png'));
    expect(names.where((n) => n.endsWith('.tmp')), isEmpty);
  });

  test('markSent removes the report and its PNG, and only that one', () async {
    final log = AppLogger(src: 'app-win');
    final outbox = Outbox.forRoot(_root);
    final a = _report(log, now: DateTime.utc(2026, 11, 2, 9));
    final b = _report(log, now: DateTime.utc(2026, 11, 2, 10));
    await outbox.add(a, screenshot: Uint8List.fromList([9]));
    await outbox.add(b);
    await outbox.markSent(a.id);
    expect((await outbox.pending()).map((i) => i.id), [b.id]);
    expect(
      _root.listSync(recursive: true).any((e) => e.path.contains(a.id)),
      isFalse,
    );
    await outbox.markSent('../escape');
    await outbox.markSent('unknown-id');
    expect((await outbox.pending()).length, 1);
    await outbox.markSent(b.id);
    expect(await outbox.pending(), isEmpty);
  });

  test('pending on a missing directory is empty and skips unreadable files', () async {
    final outbox = Outbox.forRoot(_root);
    expect(await outbox.pending(), isEmpty);
    Directory('${_root.path}/outbox').createSync();
    File('${_root.path}/outbox/20261102T134512345Z-bad.json').writeAsStringSync('{not json');
    File('${_root.path}/outbox/notes.txt').writeAsStringSync('hi');
    expect(await outbox.pending(), isEmpty);
  });

  group('report', () {
    test('has the spec shape, version 1', () {
      final log = AppLogger(src: 'app-win');
      final r = _report(log, shot: 'shot.png');
      expect(r.json['version'], 1);
      expect(r.json['created'], '2026-11-02T13:45:12.345Z');
      expect(r.json['id'], r.id);
      expect(r.id, matches(RegExp(r'^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$')));
      expect(r.json['kind'], 'bug');
      expect(r.json['app'], {'version': '0.1.0', 'build': '1', 'commit': ''});
      expect(r.json['device'], {'id': 'DEV-1', 'name': 'laptop', 'model': 'Windows 11'});
      expect(r.json['pc'], 'PC-A');
      expect(r.json['session'], isNull);
      expect(r.json['route'], 'home');
      expect(r.json['locale'], 'en-US');
      expect(r.json['screenshot'], 'shot.png');
      expect(jsonDecode(jsonEncode(r.json)), isA<Map>());
    });

    test('text and every string pass through redaction', () {
      final log = AppLogger(src: 'app-win')..registerSecret('lit-secret-value');
      final r = buildReport(
        log: log,
        now: DateTime.utc(2026, 11, 2),
        text: 'broke: token=abc123 and lit-secret-value, Bearer zzz.yyy',
        kind: FeedbackKind.complaint,
        app: const FeedbackApp(version: '0.1.0', build: '1', commit: ''),
        platform: 'android',
        device: const FeedbackDevice(id: 'D', name: 'phone lit-secret-value', model: 'm'),
        pc: null,
        session: null,
        route: 'pairing',
        locale: 'en',
        logs: ReportLogs([
          {'msg': 'old line with password=hunter2', 'fields': {'secret': 'x', 'ok': 1}},
        ], const []),
      );
      final text = jsonEncode(r.json);
      for (final s in ['abc123', 'lit-secret-value', 'zzz.yyy', 'hunter2']) {
        expect(text, isNot(contains(s)), reason: s);
      }
      expect(r.json['text'], 'broke: token=[redacted] and [redacted], Bearer [redacted]');
    });

    test('keeps at most 200 lines and 20 errors, the newest', () {
      final log = AppLogger(src: 'app-win');
      final r = buildReport(
        log: log,
        now: DateTime.utc(2026, 11, 2),
        text: '',
        kind: FeedbackKind.other,
        app: const FeedbackApp(version: 'v', build: '1', commit: ''),
        platform: 'windows',
        device: const FeedbackDevice(id: 'D', name: 'n', model: 'm'),
        pc: null,
        session: null,
        route: 'home',
        locale: 'en',
        logs: ReportLogs(
          [for (var i = 0; i < 300; i++) {'msg': 'l$i'}],
          [for (var i = 0; i < 30; i++) {'msg': 'e$i'}],
        ),
      );
      final logs = r.json['logs']! as List;
      final errors = r.json['errors']! as List;
      expect(logs.length, 200);
      expect((logs.first as Map)['msg'], 'l100');
      expect(errors.length, 20);
      expect((errors.last as Map)['msg'], 'e29');
    });
  });
}
