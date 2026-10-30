import 'package:bubbleplay_app/src/devices/devices_model.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/devices_fixtures.dart';
import '../support/harness.dart';

void main() {
  group('This PC without a pipe', () {
    testWidgets('says Not connected and offers nothing', (tester) async {
      await pumpApp(tester, contractState('home'));
      expect(find.text('Not connected'), findsOneWidget);
      expect(find.text('Pair a device'), findsNothing);
    });

    testWidgets('a narrow Windows window hides This PC', (tester) async {
      await pumpApp(tester, contractState('home'), screen: Screen.phone, windows: true);
      expect(find.text('This PC'), findsNothing);
    });
  });

  group('This PC connected', () {
    testWidgets('shows the host header, PC ID and paired devices', (tester) async {
      await pumpThisPc(tester, connectedHost());
      expect(find.text('Atelier'), findsWidgets);
      expect(find.text('Relay online'), findsWidgets);
      expect(find.text('K7QX9M2PD4WH6R3T'), findsOneWidget);
      expect(find.text('Pixel 8'), findsOneWidget);
      expect(find.text('Connected'), findsOneWidget);
      expect(find.text('Last seen 3 h ago'), findsOneWidget);
      expect(find.text('Never seen'), findsOneWidget);
      expect(find.text('Revoke'), findsNWidgets(3));
      expect(find.text('Pair a device'), findsOneWidget);
    });

    testWidgets('relay offline reads as such', (tester) async {
      await pumpThisPc(tester, connectedHost(relayOnline: false));
      expect(find.text('Relay offline'), findsWidgets);
    });

    testWidgets('Pair a device starts pairing', (tester) async {
      final app = await pumpThisPc(tester, connectedHost());
      await tester.tap(find.text('Pair a device'));
      await tester.pump();
      expect(app.sent, [
        {'type': 'startPairing'},
      ]);
    });

    testWidgets('an empty list says so', (tester) async {
      await pumpThisPc(tester, [
        {'type': 'connected'},
        helloMsg(),
        {'type': 'list', 'devices': <Object?>[]},
      ]);
      expect(find.text('No devices are paired with this PC yet.'), findsOneWidget);
    });

    testWidgets('a save error shows as an error notice', (tester) async {
      await pumpThisPc(tester, [
        ...connectedHost(),
        {'type': 'saveError', 'message': 'The trusted list could not be written.'},
      ]);
      expect(find.text('Could not save'), findsOneWidget);
      expect(find.text('The trusted list could not be written.'), findsOneWidget);
    });
  });

  group('This PC pairing', () {
    testWidgets('open pairing shows the code, QR, countdown and Cancel', (tester) async {
      final app = await pumpThisPc(tester, [...connectedHost(), pairWaitingMsg()]);
      expect(find.text('70Q1K-9MXR1'), findsOneWidget);
      expect(find.text('Expires in 4:32'), findsOneWidget);
      expect(find.textContaining('PC ID K7QX9M2PD4WH6R3T'), findsOneWidget);
      await tester.tap(find.text('Cancel pairing'));
      await tester.pump();
      expect(app.sent.single, {'type': 'cancelPairing'});
    });

    testWidgets('the countdown redraws every second', (tester) async {
      await pumpThisPc(tester, [...connectedHost(), pairWaitingMsg()]);
      await tester.pump(const Duration(seconds: 1));
      expect(find.textContaining('Expires in'), findsOneWidget);
    });

    testWidgets('a request shows the number; Allow and Deny send the id', (tester) async {
      final app = await pumpThisPc(
        tester,
        [...connectedHost(), pairWaitingMsg(), pairRequestMsg()],
      );
      expect(find.text('Galaxy Tab wants to pair'), findsOneWidget);
      expect(find.text('482  913'), findsOneWidget);
      await tester.tap(find.text('Allow'));
      await tester.pump();
      await tester.tap(find.text('Deny'));
      await tester.pump();
      expect(app.sent, [
        {'type': 'allow', 'id': 'dev-new'},
        {'type': 'deny', 'id': 'dev-new'},
      ]);
    });

    testWidgets('Deny comes before Allow', (tester) async {
      await pumpThisPc(tester, [...connectedHost(), pairRequestMsg()]);
      expect(
        tester.getTopLeft(find.text('Deny')).dx,
        lessThan(tester.getTopLeft(find.text('Allow')).dx),
      );
    });

    testWidgets('done says who paired and Done closes it', (tester) async {
      final app = await pumpThisPc(tester, [
        ...connectedHost(),
        {'type': 'pairDone', 'id': 'dev-new', 'name': 'Galaxy Tab'},
      ]);
      expect(find.text('Paired with Galaxy Tab'), findsOneWidget);
      await tester.tap(find.text('Done'));
      await tester.pump();
      expect(find.text('Pair a device'), findsOneWidget);
      expect(app.sent, isEmpty);
    });

    for (final (type, title) in [
      ('pairDenied', 'Pairing denied'),
      ('pairFailed', 'Pairing failed'),
      ('pairExpired', 'Code expired'),
      ('pairStopped', 'Pairing stopped'),
    ]) {
      testWidgets('$type says $title and offers a new attempt', (tester) async {
        final app = await pumpThisPc(tester, [
          ...connectedHost(),
          {'type': type},
        ]);
        expect(find.text(title), findsOneWidget);
        await tester.tap(find.text('Pair a device'));
        await tester.pump();
        expect(app.sent.single, {'type': 'startPairing'});
      });
    }
  });

  group('This PC revoke', () {
    testWidgets('asks first, naming the device, and Cancel sends nothing', (tester) async {
      final app = await pumpThisPc(tester, connectedHost());
      await tester.tap(find.text('Revoke').first);
      await tester.pumpAndSettle();
      expect(find.text('Revoke Pixel 8?'), findsOneWidget);
      await tester.tap(find.text('Cancel'));
      await tester.pumpAndSettle();
      expect(app.sent, isEmpty);
    });

    testWidgets('confirming sends revoke with the id', (tester) async {
      final app = await pumpThisPc(tester, connectedHost());
      await tester.tap(find.text('Revoke').at(1));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Revoke Surface Go'));
      await tester.pumpAndSettle();
      expect(app.sent, [
        {'type': 'revoke', 'id': 'dev-b'},
      ]);
    });
  });

  group('This PC not connected', () {
    final cases = {
      DisconnectReason.hostNotRunning: 'BubblePlay host is not running',
      DisconnectReason.notTrusted: 'Host not trusted',
      DisconnectReason.protocol: 'Host sent something unreadable',
      DisconnectReason.io: 'Connection to the host lost',
      DisconnectReason.unsupported: 'Host version not supported',
    };
    for (final MapEntry(key: reason, value: title) in cases.entries) {
      testWidgets('${reason.name} says $title and offers no actions', (tester) async {
        final app = await pumpThisPc(tester, [
          {'type': 'disconnected', 'reason': reason.name},
        ]);
        expect(find.text(title), findsOneWidget);
        expect(find.text('Pair a device'), findsNothing);
        expect(find.text('Revoke'), findsNothing);
        expect(app.sent, isEmpty);
      });
    }

    testWidgets('before the first line it says it is connecting', (tester) async {
      await pumpThisPc(tester, []);
      expect(find.text('Connecting to the BubblePlay host'), findsOneWidget);
    });
  });

  group('selection', () {
    testWidgets('picking a PC leaves This PC; a show event returns to it', (tester) async {
      final app = await pumpThisPc(tester, connectedHost());
      expect(find.text('Pair a device'), findsOneWidget);
      await tester.tap(find.text('Studio').first);
      await tester.pump();
      expect(find.text('Pair a device'), findsNothing);
      app.channel.emit({'type': 'show'});
      await tester.pump();
      expect(find.text('Pair a device'), findsOneWidget);
    });

    testWidgets('the This PC row selects it again', (tester) async {
      await pumpThisPc(tester, connectedHost());
      await tester.tap(find.text('Studio').first);
      await tester.pump();
      await tester.tap(find.text('3 paired devices'));
      await tester.pump();
      expect(find.text('Pair a device'), findsOneWidget);
    });
  });
}
