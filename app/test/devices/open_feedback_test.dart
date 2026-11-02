import 'dart:io';

import 'package:bubbleplay_app/src/devices/devices_controller.dart';
import 'package:bubbleplay_app/src/devices/devices_model.dart';
import 'package:bubbleplay_app/src/devices/fake_devices_channel.dart';
import 'package:bubbleplay_app/src/feedback/feedback_controller.dart';
import 'package:bubbleplay_app/src/feedback/outbox.dart';
import 'package:bubbleplay_app/src/logging/app_log.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

void main() {
  test('openFeedback decodes, and an unknown type still does not', () {
    expect(parseDevicesLine('{"type":"openFeedback"}'), isA<OpenFeedbackEvent>());
    expect(parseDevicesLine('{"type":"openFeedbackX"}'), isNull);
  });

  test('the controller signals openFeedback and keeps it pending until consumed', () async {
    final channel = FakeDevicesChannel();
    final controller = DevicesController(channel);
    addTearDown(controller.dispose);
    var requests = 0;
    final sub = controller.feedbackRequests.listen((_) => requests++);
    addTearDown(sub.cancel);
    expect(controller.feedbackPending, isFalse);
    channel.emit({'type': 'openFeedback'});
    await Future<void>.delayed(Duration.zero);
    expect(requests, 1);
    expect(controller.feedbackPending, isTrue);
    controller.consumeFeedback();
    expect(controller.feedbackPending, isFalse);
    // It does not disturb the Devices state or the show signal.
    expect(controller.showPending, isFalse);
  });

  group('in the app', () {
    late Directory root;
    late FeedbackServices services;

    setUp(() {
      root = Directory.systemTemp.createTempSync('bp-open-feedback-test');
      services = FeedbackServices(
        outbox: Outbox.forRoot(root),
        log: AppLogger(src: 'app-win'),
        capture: () async => null,
      );
    });
    tearDown(() => root.deleteSync(recursive: true));

    testWidgets('an openFeedback line opens the report sheet', (tester) async {
      final channel = FakeDevicesChannel();
      final controller = DevicesController(channel);
      addTearDown(controller.dispose);
      await pumpApp(tester, contractState('home'), devices: controller, feedback: services);
      expect(find.text('Report a problem'), findsNothing);
      channel.emit({'type': 'openFeedback'});
      await tester.pumpAndSettle();
      expect(find.text('Report a problem'), findsOneWidget);
      expect(find.text('Send'), findsOneWidget);
      expect(controller.feedbackPending, isFalse);
    });

    testWidgets('a second openFeedback while the sheet is open does not stack another', (tester) async {
      final channel = FakeDevicesChannel();
      final controller = DevicesController(channel);
      addTearDown(controller.dispose);
      await pumpApp(tester, contractState('home'), devices: controller, feedback: services);
      channel.emit({'type': 'openFeedback'});
      await tester.pumpAndSettle();
      channel.emit({'type': 'openFeedback'});
      await tester.pumpAndSettle();
      expect(find.text('Report a problem'), findsOneWidget);
    });

    testWidgets('one that arrived before the app was up opens on the first frame', (tester) async {
      final channel = FakeDevicesChannel();
      final controller = DevicesController(channel);
      addTearDown(controller.dispose);
      channel.emit({'type': 'openFeedback'});
      expect(controller.feedbackPending, isTrue);
      await pumpApp(tester, contractState('home'), devices: controller, feedback: services);
      await tester.pumpAndSettle();
      expect(find.text('Report a problem'), findsOneWidget);
      expect(controller.feedbackPending, isFalse);
    });
  });
}
