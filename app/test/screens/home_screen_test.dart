import 'package:flutter/widgets.dart' show IntrinsicWidth;
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

void main() {
  group('home on desktop', () {
    testWidgets('lists Your PCs with This PC pinned above them', (tester) async {
      await pumpApp(tester, contractState('home'));
      expect(find.text('This PC'), findsOneWidget);
      expect(find.text('Your PCs'), findsOneWidget);
      expect(find.text('Studio'), findsWidgets);
      expect(find.text('Pair a new PC'), findsOneWidget);
      expect(find.text('Settings'), findsOneWidget);
    });

    testWidgets('shows the host in This PC when there is one', (tester) async {
      await pumpApp(tester, contractState('home_with_host'));
      final host = contractState('home_with_host')! as Map;
      expect(find.text((host['host'] as Map)['pc_name'] as String), findsOneWidget);
    });

    testWidgets('says This PC is not connected without a host', (tester) async {
      await pumpApp(tester, contractState('home'));
      expect(find.text('Not connected'), findsOneWidget);
    });

    testWidgets('Connect sends the Connect action for that PC', (tester) async {
      final app = await pumpApp(tester, contractState('home'));
      await tester.tap(find.text('Connect to Studio'));
      await tester.pump();
      expect(app.actions.single, {
        'Connect': {'pc_id': isA<String>()},
      });
    });

    testWidgets('Pair a new PC is as wide as its label, not the header', (tester) async {
      await pumpApp(tester, contractState('home'));
      final button = find.ancestor(
        of: find.text('Pair a new PC'),
        matching: find.byType(IntrinsicWidth),
      );
      expect(tester.getSize(button.first).width, lessThan(300));
    });

    testWidgets('Pair a new PC starts pairing', (tester) async {
      final app = await pumpApp(tester, contractState('home'));
      await tester.tap(find.text('Pair a new PC'));
      await tester.pump();
      expect(app.actions.single, 'StartPairing');
    });

    testWidgets('an empty list offers to pair', (tester) async {
      final app = await pumpApp(tester, contractState('home_empty'));
      expect(find.text('No PCs yet'), findsOneWidget);
      await tester.tap(find.text('Pair a PC'));
      await tester.pump();
      expect(app.actions.single, 'StartPairing');
    });

    testWidgets('a PC with a problem says so on its row and detail', (tester) async {
      await pumpApp(tester, contractState('home_problems'));
      expect(find.text('Relay unreachable'), findsWidgets);
    });
  });

  group('home on a phone', () {
    testWidgets('has no This PC section on Android', (tester) async {
      await pumpApp(tester, contractState('home'), screen: Screen.phone, windows: false);
      expect(find.text('This PC'), findsNothing);
      expect(find.text('Your PCs'), findsOneWidget);
      expect(find.text('Pair a PC'), findsOneWidget);
    });

    testWidgets('tapping a row connects', (tester) async {
      final app = await pumpApp(tester, contractState('home'), screen: Screen.phone, windows: false);
      await tester.tap(find.text('Studio'));
      await tester.pump();
      expect(app.actions.single, {
        'Connect': {'pc_id': isA<String>()},
      });
    });

    testWidgets('the actions button opens Rename and Forget', (tester) async {
      await pumpApp(tester, contractState('home'), screen: Screen.phone, windows: false);
      await tester.tap(find.bySemanticsLabel('Actions for Studio'));
      await tester.pumpAndSettle();
      expect(find.text('Rename'), findsOneWidget);
      expect(find.text('Forget'), findsOneWidget);
    });
  });
}
