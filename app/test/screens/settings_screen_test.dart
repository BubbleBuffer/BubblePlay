import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

const _deviceId = 'AB12-CD34-EF56-GH78';

String _keyPath() =>
    ((contractState('home')! as Map)['this_device'] as Map)['key_path'] as String;

/// Taps the Settings button on the PC list and waits for the panel or page.
Future<void> _openSettings(WidgetTester tester) async {
  await tester.tap(find.text('Settings'));
  await tester.pumpAndSettle();
}

/// The panel's close button (the barrier also labels itself Close).
final _closeButton = find.byWidgetPredicate(
  (widget) => widget is IconBtn && widget.icon == Ico.close,
);

/// Taps the theme select and picks [name] in the popover.
Future<void> _pickTheme(WidgetTester tester, String current, String name) async {
  await tester.tap(find.text(current));
  await tester.pumpAndSettle();
  await tester.tap(find.text(name));
  await tester.pumpAndSettle();
}

void main() {
  group('settings on desktop', () {
    testWidgets('the panel shows this device\'s ID and key file', (tester) async {
      final theme = ValueNotifier<Brightness?>(null);
      addTearDown(theme.dispose);
      await pumpApp(tester, contractState('home'), theme: theme);
      await _openSettings(tester);
      // The footer of the PC list behind the panel says "This device" too.
      expect(find.text('This device'), findsWidgets);
      expect(find.text('Device ID'), findsOneWidget);
      // The PC list's footer shows the same ID behind the panel.
      expect(find.text(_deviceId), findsWidgets);
      expect(find.text('Key file'), findsOneWidget);
      expect(find.text(_keyPath()), findsOneWidget);
      expect(find.text('Appearance'), findsOneWidget);
      expect(find.text('About'), findsOneWidget);
    });

    testWidgets('Export saves the key and confirms it', (tester) async {
      final app = await pumpApp(tester, contractState('home'));
      await _openSettings(tester);
      await tester.tap(find.text('Export'));
      await tester.pump();
      await tester.pump();
      expect(
        app.keyFiles.saved['bubbleplay-device-key.bpkey'],
        app.channel.keyText,
      );
      expect(find.text('Key file saved'), findsOneWidget);
    });

    testWidgets('the theme select sets Dark, Light and System', (tester) async {
      final theme = ValueNotifier<Brightness?>(null);
      addTearDown(theme.dispose);
      await pumpApp(tester, contractState('home'), theme: theme);
      await _openSettings(tester);
      expect(find.text('Theme'), findsOneWidget);
      expect(find.text('System'), findsOneWidget);

      await _pickTheme(tester, 'System', 'Dark');
      expect(theme.value, Brightness.dark);

      await _pickTheme(tester, 'Dark', 'Light');
      expect(theme.value, Brightness.light);

      await _pickTheme(tester, 'Light', 'System');
      expect(theme.value, isNull);
    });

    testWidgets('without a theme notifier there is no Theme row', (tester) async {
      await pumpApp(tester, contractState('home'));
      await _openSettings(tester);
      expect(find.text('Appearance'), findsOneWidget);
      expect(find.text('Theme'), findsNothing);
      expect(find.text('System'), findsNothing);
    });

    testWidgets('About gives the version', (tester) async {
      await pumpApp(tester, contractState('home'));
      await _openSettings(tester);
      expect(find.text('Version'), findsOneWidget);
      expect(find.text('0.1.0'), findsOneWidget);
      expect(find.text('Stored on this device'), findsOneWidget);
    });

    testWidgets('the close button closes the panel', (tester) async {
      await pumpApp(tester, contractState('home'));
      await _openSettings(tester);
      await tester.tap(_closeButton);
      await tester.pumpAndSettle();
      expect(find.text('Device ID'), findsNothing);
      expect(find.text('Your PCs'), findsOneWidget);
    });

    testWidgets('Escape closes the panel', (tester) async {
      await pumpApp(tester, contractState('home'));
      await _openSettings(tester);
      await tester.sendKeyEvent(LogicalKeyboardKey.escape);
      await tester.pumpAndSettle();
      expect(find.text('Device ID'), findsNothing);
      expect(find.text('Your PCs'), findsOneWidget);
    });
  });

  group('settings on a phone', () {
    testWidgets('the page shows the same facts', (tester) async {
      await pumpApp(tester, contractState('home'), screen: Screen.phone, windows: false);
      await _openSettings(tester);
      expect(find.text('This device'), findsOneWidget);
      expect(find.text('Device ID'), findsOneWidget);
      expect(find.text(_deviceId), findsOneWidget);
      expect(find.text('Key file'), findsOneWidget);
      expect(find.text(_keyPath()), findsOneWidget);
      expect(find.text('Version'), findsOneWidget);
      expect(find.text('0.1.0'), findsOneWidget);
    });

    testWidgets('Export saves the key on a phone too', (tester) async {
      final app = await pumpApp(
        tester,
        contractState('home'),
        screen: Screen.phone,
        windows: false,
      );
      await _openSettings(tester);
      await tester.tap(find.text('Export'));
      await tester.pump();
      await tester.pump();
      expect(
        app.keyFiles.saved['bubbleplay-device-key.bpkey'],
        app.channel.keyText,
      );
      expect(find.text('Key file saved'), findsOneWidget);
    });

    testWidgets('the back arrow closes the page', (tester) async {
      await pumpApp(tester, contractState('home'), screen: Screen.phone, windows: false);
      await _openSettings(tester);
      await tester.tap(find.bySemanticsLabel('Back'));
      await tester.pumpAndSettle();
      expect(find.text('Device ID'), findsNothing);
      expect(find.text('Your PCs'), findsOneWidget);
    });
  });
}
