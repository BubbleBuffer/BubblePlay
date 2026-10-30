import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

/// A finder for text inside the open strip (the tab shows some of the same words).
Finder inStrip(String text) =>
    find.descendant(of: find.byType(EdgeStrip), matching: find.text(text));

/// Pumps the session and opens the strip by tapping the tab.
Future<TestApp> openStrip(
  WidgetTester tester,
  Object? state, {
  Screen screen = Screen.desktop,
  bool windows = true,
}) async {
  final app = await pumpApp(tester, state, screen: screen, windows: windows);
  await tester.tap(find.byType(EdgeTab));
  await tester.pumpAndSettle();
  return app;
}

final Finder blackArea = find.byWidgetPredicate(
  (widget) => widget is ColoredBox && widget.color == const Color(0xFF000000),
);

void main() {
  group('the placeholder session', () {
    testWidgets('fills the screen with black and says there is no video', (tester) async {
      await pumpApp(tester, contractState('session'));
      expect(blackArea, findsOneWidget);
      expect(find.text('No video in this version.'), findsOneWidget);
      expect(find.byType(EdgeTab), findsOneWidget);
      expect(find.byType(EdgeStrip), findsNothing);
    });

    testWidgets('shows the same on a phone', (tester) async {
      await pumpApp(tester, contractState('session'), screen: Screen.phone, windows: false);
      expect(blackArea, findsOneWidget);
      expect(find.text('No video in this version.'), findsOneWidget);
      expect(find.byType(EdgeTab), findsOneWidget);
    });

    testWidgets('the tab names the PC and its mode', (tester) async {
      await pumpApp(tester, contractState('session'));
      expect(find.text('Studio'), findsOneWidget);
      expect(find.text('Game'), findsOneWidget);
    });

    testWidgets('tapping the tab opens the strip', (tester) async {
      await openStrip(tester, contractState('session'));
      expect(find.byType(EdgeStrip), findsOneWidget);
      expect(inStrip('Work'), findsOneWidget);
      expect(inStrip('Game'), findsOneWidget);
      expect(inStrip('Display'), findsOneWidget);
      expect(inStrip('Leave'), findsOneWidget);
    });

    testWidgets('the strip opens on a phone too', (tester) async {
      await openStrip(tester, contractState('session'), screen: Screen.phone, windows: false);
      expect(find.byType(EdgeStrip), findsOneWidget);
      expect(inStrip('Work'), findsOneWidget);
    });
  });

  group('session actions', () {
    testWidgets('Work sends SetMode Work', (tester) async {
      final app = await openStrip(tester, contractState('session'));
      await tester.tap(inStrip('Work'));
      await tester.pump();
      expect(app.actions.single, {'SetMode': 'Work'});
    });

    testWidgets('Game sends SetMode Game', (tester) async {
      final app = await openStrip(tester, contractState('session'));
      await tester.tap(inStrip('Game'));
      await tester.pump();
      expect(app.actions.single, {'SetMode': 'Game'});
    });

    testWidgets('Leave sends Disconnect', (tester) async {
      final app = await openStrip(tester, contractState('session'));
      await tester.tap(inStrip('Leave'));
      await tester.pump();
      expect(app.actions.single, 'Disconnect');
    });

    testWidgets('Escape opens and closes the strip', (tester) async {
      final app = await pumpApp(tester, contractState('session'));
      await tester.sendKeyEvent(LogicalKeyboardKey.escape);
      await tester.pumpAndSettle();
      expect(find.byType(EdgeStrip), findsOneWidget);
      await tester.sendKeyEvent(LogicalKeyboardKey.escape);
      await tester.pumpAndSettle();
      expect(find.byType(EdgeStrip), findsNothing);
      expect(app.actions, isEmpty);
    });
  });

  group('controls that are not available', () {
    testWidgets('Audio sends nothing when it changes nothing', (tester) async {
      final app = await openStrip(tester, contractState('session_placeholder'));
      await tester.tap(inStrip('Audio'));
      await tester.pump();
      expect(app.actions, isEmpty);
    });

    testWidgets('Quality sends nothing when it changes nothing', (tester) async {
      final app = await openStrip(tester, contractState('session_placeholder'));
      await tester.tap(inStrip('Sharp'));
      await tester.pump();
      expect(app.actions, isEmpty);
    });

    testWidgets('Display is disabled without monitors', (tester) async {
      final app = await openStrip(tester, contractState('session_placeholder'));
      await tester.tap(inStrip('Display'));
      await tester.pumpAndSettle();
      expect(find.byType(DisplayPicker), findsNothing);
      expect(app.actions, isEmpty);
    });

    testWidgets('Audio sends SetAudio true when sound is off and can change', (tester) async {
      final state = contractState('session_placeholder', (snapshot) {
        final session = snapshot['session']! as Map<String, Object?>;
        session['audio'] = false;
        session['audio_controls_available'] = true;
      });
      final app = await openStrip(tester, state);
      await tester.tap(inStrip('Audio'));
      await tester.pump();
      expect(app.actions.single, {'SetAudio': true});
    });

    testWidgets('Quality sends SetQuality Sharp when it can change', (tester) async {
      final state = contractState('session', (snapshot) {
        final session = snapshot['session']! as Map<String, Object?>;
        session['quality'] = 'Auto';
        session['quality_controls_available'] = true;
      });
      final app = await openStrip(tester, state);
      await tester.tap(inStrip('Sharp'));
      await tester.pump();
      expect(app.actions.single, {'SetQuality': 'Sharp'});
    });
  });

  group('the display picker', () {
    testWidgets('lists the monitors and picking one sends SetMonitor', (tester) async {
      final app = await openStrip(tester, contractState('session'));
      await tester.tap(inStrip('Display'));
      await tester.pumpAndSettle();
      expect(find.byType(DisplayPicker), findsOneWidget);
      expect(find.text('Display 2'), findsOneWidget);
      await tester.tap(find.text('Display 2'));
      await tester.pumpAndSettle();
      expect(app.actions.single, {'SetMonitor': 1});
      expect(find.byType(DisplayPicker), findsNothing);
    });
  });

  group('the stats panel', () {
    testWidgets('Stats opens the stream metrics', (tester) async {
      await openStrip(tester, contractState('session'));
      await tester.tap(inStrip('Stats'));
      await tester.pumpAndSettle();
      expect(find.byType(MetricsPanel), findsOneWidget);
      expect(find.text('Stream'), findsOneWidget);
      expect(find.text('2560x1440'), findsOneWidget);
      expect(find.text('59.5 fps'), findsOneWidget);
      expect(find.text('24.5 Mb/s'), findsOneWidget);
      expect(find.text('18 ms'), findsOneWidget);
      expect(find.text('2.5 ms'), findsOneWidget);
      expect(find.text('WebRTC'), findsOneWidget);
      expect(find.text('H.264'), findsOneWidget);
      expect(find.text('Dropped'), findsOneWidget);
    });

    testWidgets('empty values read as a dash', (tester) async {
      await openStrip(tester, contractState('session_placeholder'));
      await tester.tap(inStrip('Stats'));
      await tester.pumpAndSettle();
      expect(find.text('-'), findsNWidgets(3));
    });
  });

  group('what the session says', () {
    testWidgets('a reconnecting link says which attempt it is on', (tester) async {
      final state = contractState('session', (snapshot) {
        final session = snapshot['session']! as Map<String, Object?>;
        session['link'] = {
          'Reconnecting': {'attempt': 3, 'secs': 6},
        };
        session['notice'] = null;
      });
      await pumpApp(tester, state);
      expect(find.text('Reconnecting, attempt 3'), findsOneWidget);
    });

    testWidgets('AdminRequired says input is blocked until Admin mode is on', (tester) async {
      await pumpApp(tester, contractState('session_placeholder'));
      expect(
        find.text('Input to an elevated window is blocked until Admin mode is on.'),
        findsOneWidget,
      );
    });

    testWidgets('SecureDesktop says the PC shows a sign-in or permission prompt', (tester) async {
      await pumpApp(tester, contractState('session'));
      expect(
        find.text('The PC is showing the sign-in screen or a permission prompt.'),
        findsOneWidget,
      );
    });

    testWidgets('a live session with no notice shows no banner', (tester) async {
      final state = contractState('session', (snapshot) {
        final session = snapshot['session']! as Map<String, Object?>;
        session['link'] = 'Live';
        session['notice'] = null;
      });
      await pumpApp(tester, state);
      expect(find.byType(NoticeBanner), findsNothing);
    });
  });
}
