import 'dart:convert';
import 'dart:io';

import 'package:bubbleplay_app/src/feedback/feedback_controller.dart';
import 'package:bubbleplay_app/src/feedback/outbox.dart';
import 'package:bubbleplay_app/src/logging/app_log.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

late Directory _root;
late Outbox _outbox;
late AppLogger _log;
late int _captures;

/// A 1x1 PNG.
final _png = Uint8List.fromList(base64Decode(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
));

FeedbackServices _services({Uint8List? screenshot}) => FeedbackServices(
      outbox: _outbox,
      log: _log,
      capture: () async {
        _captures++;
        return screenshot;
      },
    );

Future<void> _ctrlShiftF(WidgetTester tester, {bool settle = true}) async {
  await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
  await tester.sendKeyDownEvent(LogicalKeyboardKey.shiftLeft);
  await tester.sendKeyEvent(LogicalKeyboardKey.keyF);
  await tester.sendKeyUpEvent(LogicalKeyboardKey.shiftLeft);
  await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
  if (settle) {
    await tester.pumpAndSettle();
  } else {
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 600));
  }
}

/// Lets real file work finish: the outbox writes with dart:io, whose steps each continue in the
/// test's fake-async zone, so real time and pumps alternate.
Future<void> _io(WidgetTester tester) async {
  for (var i = 0; i < 12; i++) {
    await tester.runAsync(() => Future<void>.delayed(const Duration(milliseconds: 30)));
    await tester.pump();
  }
  await tester.pump(const Duration(milliseconds: 600));
  await tester.pump();
  await tester.pump(const Duration(milliseconds: 100));
}

Future<List<OutboxItem>> _pending(WidgetTester tester) async =>
    (await tester.runAsync(_outbox.pending))!;

void main() {
  setUp(() {
    _root = Directory.systemTemp.createTempSync('bp-feedback-test');
    _outbox = Outbox.forRoot(_root);
    _log = AppLogger(src: 'app-win');
    _captures = 0;
  });
  tearDown(() => _root.deleteSync(recursive: true));

  testWidgets('Ctrl+Shift+F opens the sheet', (tester) async {
    await pumpApp(tester, contractState('home'), feedback: _services());
    expect(find.text('Report a problem'), findsNothing);
    await _ctrlShiftF(tester);
    expect(find.text('Report a problem'), findsOneWidget);
    expect(find.text('What happened?'), findsWidgets);
    expect(find.text('Send'), findsOneWidget);
    expect(find.text('Cancel'), findsOneWidget);
    for (final kind in ['Bug']) {
      expect(find.text(kind), findsOneWidget);
    }
  });

  testWidgets('the shortcut also works on a phone, and a second press does not stack sheets',
      (tester) async {
    await pumpApp(tester, contractState('home'),
        screen: Screen.phone, windows: false, feedback: _services());
    await _ctrlShiftF(tester);
    await _ctrlShiftF(tester);
    expect(find.text('Report a problem'), findsOneWidget);
  });

  testWidgets('Send stores the report in the outbox, redacted, and says so', (tester) async {
    _log.registerSecret('my-literal-secret');
    await pumpApp(tester, contractState('home'), feedback: _services());
    await _ctrlShiftF(tester);
    await tester.enterText(
      find.byKey(const ValueKey('feedback-text')),
      'Stream froze. token=abc123 my-literal-secret',
    );
    await tester.pump();
    await tester.tap(find.byKey(const ValueKey('feedback-send')));
    await _io(tester);
    final items = await _pending(tester);
    expect(items.length, 1);
    final report = items.single.report;
    expect(report['version'], 1);
    expect(report['kind'], 'bug');
    expect(report['text'], 'Stream froze. token=[redacted] [redacted]');
    expect(report['route'], 'home');
    expect(report['platform'], Platform.operatingSystem);
    expect((report['device']! as Map)['id'], 'AB12-CD34-EF56-GH78');
    expect(report['screenshot'], isNull);
    expect(jsonEncode(report), isNot(contains('abc123')));
    expect(jsonEncode(report), isNot(contains('my-literal-secret')));
    // The sheet is gone and the state line is shown.
    expect(find.text('Report a problem'), findsNothing);
    expect(find.text('Saved. It is sent to your PC the next time you connect.'), findsOneWidget);
    // Let the toast time out before the test ends.
    await tester.pump(const Duration(seconds: 10));
    await tester.pumpAndSettle();
  });

  testWidgets('the kind is chosen from the four names and stored', (tester) async {
    await pumpApp(tester, contractState('home'), feedback: _services());
    await _ctrlShiftF(tester);
    await tester.tap(find.byKey(const ValueKey('feedback-kind')));
    await tester.pumpAndSettle();
    for (final label in ['Bug', 'Complaint', 'Idea', 'Other']) {
      expect(find.text(label), findsWidgets, reason: label);
    }
    await tester.tap(find.byKey(const ValueKey('option-Idea')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('feedback-send')));
    await _io(tester);
    expect((await _pending(tester)).single.report['kind'], 'idea');
    await tester.pump(const Duration(seconds: 10));
    await tester.pumpAndSettle();
  });

  testWidgets('Cancel stores nothing', (tester) async {
    await pumpApp(tester, contractState('home'), feedback: _services());
    await _ctrlShiftF(tester);
    await tester.enterText(find.byKey(const ValueKey('feedback-text')), 'never mind');
    await tester.tap(find.byKey(const ValueKey('feedback-cancel')));
    await _io(tester);
    expect(find.text('Report a problem'), findsNothing);
    expect(await _pending(tester), isEmpty);
    expect(Directory('${_root.path}/outbox').existsSync(), isFalse);
  });

  testWidgets('Escape closes the sheet and stores nothing', (tester) async {
    await pumpApp(tester, contractState('home'), feedback: _services());
    await _ctrlShiftF(tester);
    expect(find.text('Report a problem'), findsOneWidget);
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await _io(tester);
    expect(find.text('Report a problem'), findsNothing);
    expect(await _pending(tester), isEmpty);
  });

  testWidgets('the screenshot is stored beside the report and can be removed first',
      (tester) async {
    await pumpApp(tester, contractState('home'), feedback: _services(screenshot: _png));
    await _ctrlShiftF(tester);
    expect(find.byKey(const ValueKey('feedback-screenshot')), findsOneWidget);
    await tester.tap(find.byKey(const ValueKey('feedback-send')));
    await _io(tester);
    var item = (await _pending(tester)).single;
    expect(item.png, isNotNull);
    expect(item.report['screenshot'], item.png!.uri.pathSegments.last);
    expect(await tester.runAsync(item.png!.readAsBytes), _png);
    await tester.pump(const Duration(seconds: 10));
    await tester.pumpAndSettle();

    await tester.runAsync(() => _outbox.markSent(item.id));
    await _ctrlShiftF(tester);
    await tester.tap(find.text('Remove screenshot'));
    await tester.pumpAndSettle();
    expect(find.byKey(const ValueKey('feedback-screenshot')), findsNothing);
    await tester.tap(find.byKey(const ValueKey('feedback-send')));
    await _io(tester);
    item = (await _pending(tester)).single;
    expect(item.png, isNull);
    expect(item.report['screenshot'], isNull);
    await tester.pump(const Duration(seconds: 10));
    await tester.pumpAndSettle();
  });

  testWidgets('no screenshot is taken while a session is showing', (tester) async {
    await pumpApp(tester, contractState('session'), feedback: _services(screenshot: _png));
    await _ctrlShiftF(tester, settle: false);
    expect(find.text('Report a problem'), findsOneWidget);
    expect(_captures, 0);
    expect(find.byKey(const ValueKey('feedback-screenshot')), findsNothing);
    await tester.tap(find.byKey(const ValueKey('feedback-send')));
    await _io(tester);
    final report = (await _pending(tester)).single.report;
    expect(report['route'], 'session');
    expect(report['pc'], isNotNull);
    expect(report['screenshot'], isNull);
    await tester.pump(const Duration(seconds: 10));

  });

  testWidgets('when the outbox cannot be written Send says so and keeps the text',
      (tester) async {
    // A regular file where the outbox directory should go: creating it fails.
    final blocker = File('${_root.path}/blocker')..writeAsStringSync('x');
    final broken = FeedbackServices(
      outbox: Outbox(Directory('${blocker.path}/outbox')),
      log: _log,
      capture: () async => null,
    );
    await pumpApp(tester, contractState('home'), feedback: broken);
    await _ctrlShiftF(tester);
    await tester.enterText(find.byKey(const ValueKey('feedback-text')), 'keep me');
    await tester.tap(find.byKey(const ValueKey('feedback-send')));
    await _io(tester);
    expect(find.text('The report could not be saved on this device.'), findsOneWidget);
    expect(find.text('keep me'), findsOneWidget);
  });

  testWidgets('Settings has a Report a problem row that opens the sheet', (tester) async {
    await pumpApp(tester, contractState('home'), feedback: _services());
    await tester.tap(find.text('Settings'));
    await tester.pumpAndSettle();
    // The row is last, below the fold of the panel: scroll the settings list to it.
    await tester.drag(find.text('Device ID'), const Offset(0, -800));
    await tester.pumpAndSettle();
    expect(find.text('Report a problem'), findsOneWidget);
    await tester.pump();
    await tester.tap(find.text('Report'));
    await tester.pumpAndSettle();
    expect(find.text('Report a problem'), findsNWidgets(2), reason: 'the row and the sheet title');
    expect(find.text('What happened?'), findsWidgets);
    await tester.tap(find.byKey(const ValueKey('feedback-cancel')));
    await tester.pumpAndSettle();
    expect(await _pending(tester), isEmpty);
  });
}
