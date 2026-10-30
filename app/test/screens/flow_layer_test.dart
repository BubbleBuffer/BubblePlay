import 'package:bb_ui/bb_ui.dart';
import 'package:bubbleplay_app/src/screens/flow_layer.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

void main() {
  group('pairing over the PC list on desktop', () {
    testWidgets('pair_enter shows the four fields and Pair over the list', (tester) async {
      await pumpApp(tester, contractState('pair_enter'));
      expect(find.text('Your PCs'), findsOneWidget);
      expect(find.text('Pair a new PC'), findsWidgets);
      expect(find.text('Pairing link (optional)'), findsOneWidget);
      expect(find.text('PC ID'), findsOneWidget);
      expect(find.text('One-time code'), findsOneWidget);
      expect(find.text('Relay'), findsWidgets);
      expect(find.text('Pair'), findsOneWidget);
      expect(find.text('Cancel'), findsOneWidget);
    });

    testWidgets('a pasted lower-case PC ID and code are shown in upper case', (tester) async {
      await pumpApp(tester, contractState('pair_enter'));
      await tester.enterText(find.byType(CodeField).first, '77vh7m568gjky12j');
      await tester.enterText(find.byType(CodeField).at(1), '70q1k9mxr1');
      await tester.pump();
      // The PC list behind the panel shows the same ID on the selected row and its details.
      expect(find.text('77VH-7M56-8GJK-Y12J'), findsWidgets);
      expect(find.text('70Q1K-9MXR1'), findsOneWidget);
    });

    testWidgets('Pair sends the PC ID, the code and the relay', (tester) async {
      final app = await pumpApp(tester, contractState('pair_enter'));
      await tester.enterText(find.byType(CodeField).first, '77vh7m568gjky12j');
      await tester.enterText(find.byType(CodeField).at(1), '70q1k9mxr1');
      await tester.enterText(find.byType(SettingsField).at(1), 'https://relay.example');
      await tester.pump();
      await tester.tap(find.text('Pair'));
      await tester.pump();
      expect(app.actions.single, {
        'SubmitPairing': {
          'link': null,
          'pc_id': '77VH-7M56-8GJK-Y12J',
          'code': '70Q1K-9MXR1',
          'relay': 'https://relay.example',
        },
      });
    });

    testWidgets('a typed pairing link is sent with the rest', (tester) async {
      final app = await pumpApp(tester, contractState('pair_enter'));
      await tester.enterText(find.byType(SettingsField).first, 'https://relay.example/pair#abc');
      await tester.pump();
      await tester.tap(find.text('Pair'));
      await tester.pump();
      expect([app.actions.single], [
        {
          'SubmitPairing': {
            'link': 'https://relay.example/pair#abc',
            'pc_id': '',
            'code': '',
            'relay': '',
          },
        },
      ]);
    });

    testWidgets('Cancel sends CancelFlow', (tester) async {
      final app = await pumpApp(tester, contractState('pair_enter'));
      await tester.tap(find.text('Cancel'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('Escape sends CancelFlow', (tester) async {
      final app = await pumpApp(tester, contractState('pair_enter'));
      await tester.sendKeyEvent(LogicalKeyboardKey.escape);
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('Pair without a code switches the input to a remote request', (tester) async {
      final app = await pumpApp(tester, contractState('pair_enter'));
      await tester.tap(find.text('Pair without a code'));
      await tester.pump();
      expect(app.actions.single, 'StartRemotePairing');
    });

    testWidgets('the remote form shows its error and Send request sends a request', (tester) async {
      final app = await pumpApp(tester, contractState('pair_enter_remote_error'));
      expect(find.text('Remote pairing is not available in this version.'), findsOneWidget);
      expect(find.text('One-time code'), findsNothing);
      await tester.enterText(find.byType(CodeField).first, '77vh7m568gjky12j');
      await tester.enterText(find.byType(SettingsField).first, 'https://relay.example');
      await tester.pump();
      await tester.tap(find.text('Send request'));
      await tester.pump();
      expect(app.actions.single, {
        'RequestRemotePairing': {
          'pc_id': '77VH-7M56-8GJK-Y12J',
          'relay': 'https://relay.example',
        },
      });
    });

    testWidgets('pair_waiting shows the number to compare and can be cancelled', (tester) async {
      final app = await pumpApp(tester, contractState('pair_waiting'));
      expect(find.text('Compare the number'), findsOneWidget);
      expect(find.text('Studio'), findsWidgets);
      expect(find.text('77VH-7M56-8GJK-Y12J'), findsWidgets);
      expect(find.text('482  913'), findsOneWidget);
      expect(find.text('Waiting for the PC to confirm'), findsOneWidget);
      expect(find.text('42 s'), findsOneWidget);
      expect(find.text('The same number must show on the PC.'), findsOneWidget);
      await tester.tap(find.text('Cancel'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('pair_approved_pc says the PC approved it', (tester) async {
      final app = await pumpApp(tester, contractState('pair_approved_pc'));
      expect(find.text('Paired'), findsOneWidget);
      expect(find.text('Approved at the PC.'), findsOneWidget);
      await tester.tap(find.text('Done'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('pair_approved_device names the device that approved it', (tester) async {
      final app = await pumpApp(tester, contractState('pair_approved_device'));
      expect(find.text('Paired'), findsOneWidget);
      expect(find.text('Approved by Laptop.'), findsOneWidget);
      await tester.tap(find.text('Done'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('pair_denied says who denied it', (tester) async {
      final app = await pumpApp(tester, contractState('pair_denied'));
      expect(find.text('Pairing denied'), findsOneWidget);
      expect(find.text('Denied at the PC.'), findsOneWidget);
      await tester.tap(find.text('Close'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('pair_timed_out explains why nobody answered', (tester) async {
      final app = await pumpApp(tester, contractState('pair_timed_out'));
      expect(find.text('Pairing timed out'), findsOneWidget);
      expect(find.text('Nobody answered before the request expired.'), findsOneWidget);
      await tester.tap(find.text('Close'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('pair_cancelled says the window on the PC was closed', (tester) async {
      final app = await pumpApp(tester, contractState('pair_cancelled'));
      expect(find.text('Pairing closed'), findsOneWidget);
      expect(find.text('The pairing window on the PC was closed.'), findsOneWidget);
      await tester.tap(find.text('Close'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('approval falls back to Not available yet', (tester) async {
      final app = await pumpApp(tester, contractState('approval'));
      expect(find.text('Not available yet'), findsOneWidget);
      expect(
        find.text('This step needs streaming, which this version does not have.'),
        findsOneWidget,
      );
      await tester.tap(find.text('Close'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });
  });

  group('pairing on a phone', () {
    testWidgets('the input takes the screen and Pair sends what was typed', (tester) async {
      final app = await pumpApp(
        tester,
        contractState('pair_enter'),
        screen: Screen.phone,
        windows: false,
      );
      expect(find.text('Pair a new PC'), findsWidgets);
      expect(find.text('One-time code'), findsOneWidget);
      await tester.enterText(find.byType(CodeField).first, '77vh7m568gjky12j');
      await tester.enterText(find.byType(CodeField).at(1), '70q1k9mxr1');
      await tester.enterText(find.byType(SettingsField).at(1), 'https://relay.example');
      await tester.pump();
      expect(find.text('77VH-7M56-8GJK-Y12J'), findsWidgets);
      await tester.tap(find.text('Pair'));
      await tester.pump();
      expect(app.actions.single, {
        'SubmitPairing': {
          'link': null,
          'pc_id': '77VH-7M56-8GJK-Y12J',
          'code': '70Q1K-9MXR1',
          'relay': 'https://relay.example',
        },
      });
    });

    testWidgets('the back arrow cancels the flow', (tester) async {
      final app = await pumpApp(
        tester,
        contractState('pair_waiting'),
        screen: Screen.phone,
        windows: false,
      );
      expect(find.text('Compare the number'), findsWidgets);
      expect(find.text('482  913'), findsOneWidget);
      await tester.tap(
        find.descendant(of: find.byType(FlowLayer), matching: find.byType(IconBtn)),
      );
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('the closing button on a finished wait cancels the flow', (tester) async {
      final app = await pumpApp(
        tester,
        contractState('pair_approved_pc'),
        screen: Screen.phone,
        windows: false,
      );
      expect(find.text('Paired'), findsOneWidget);
      await tester.tap(find.text('Done'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });
  });
}
