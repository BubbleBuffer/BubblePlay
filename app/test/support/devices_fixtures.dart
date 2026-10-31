import 'package:bubbleplay_app/src/devices/devices_controller.dart';
import 'package:bubbleplay_app/src/devices/fake_devices_channel.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import 'harness.dart';

/// The clock every Devices test runs on.
final DateTime devicesNow = DateTime.utc(2026, 10, 30, 12);

/// Host messages, as the pipe sends them (docs/app/devices-pipe.md).
Map<String, Object?> helloMsg({bool relayOnline = true}) => {
      'type': 'hello',
      'protocol': 1,
      'pcName': 'Atelier',
      'pcId': 'K7QX9M2PD4WH6R3T',
      'relayOrigin': 'https://relay.example',
      'relayOnline': relayOnline,
    };

Map<String, Object?> listMsg() => {
      'type': 'list',
      'devices': [
        {
          'id': 'dev-a',
          'name': 'Pixel 8',
          'lastSeen': devicesNow.subtract(const Duration(minutes: 5)).toIso8601String(),
          'connected': true,
        },
        {
          'id': 'dev-b',
          'name': 'Surface Go',
          'lastSeen': devicesNow.subtract(const Duration(hours: 3)).toIso8601String(),
          'connected': false,
        },
        {'id': 'dev-c', 'name': 'Old tablet', 'lastSeen': null, 'connected': false},
      ],
    };

Map<String, Object?> pairWaitingMsg() => {
      'type': 'pairWaiting',
      'codeDisplay': '70Q1K-9MXR1',
      'qrUri': 'bubbleplay://pair?pc=K7QX9M2PD4WH6R3T&code=70Q1K9MXR1',
      'pcId': 'K7QX9M2PD4WH6R3T',
      'expiresAtMs': devicesNow.add(const Duration(seconds: 272)).millisecondsSinceEpoch,
    };

Map<String, Object?> pairRequestMsg() =>
    {'type': 'pairRequest', 'id': 'dev-new', 'name': 'Galaxy Tab', 'sasDisplay': '482 913'};

/// A connected, listed host.
List<Map<String, Object?>> connectedHost({bool relayOnline = true}) => [
      {'type': 'connected'},
      helloMsg(relayOnline: relayOnline),
      listMsg(),
    ];

/// A running Devices UI over a fake channel.
class DevicesApp {
  DevicesApp(this.app, this.channel, this.controller);

  final TestApp app;
  final FakeDevicesChannel channel;
  final DevicesController controller;

  /// Commands sent to the host, in order.
  List<Map<String, Object?>> get sent => channel.sent;
}

/// Pumps the app on Windows with This PC selected, after feeding [messages] to the controller.
Future<DevicesApp> pumpThisPc(
  WidgetTester tester,
  List<Map<String, Object?>> messages, {
  Brightness brightness = Brightness.dark,
}) async {
  final channel = FakeDevicesChannel();
  final controller = DevicesController(channel, now: () => devicesNow);
  addTearDown(controller.dispose);
  for (final message in messages) {
    channel.emit(message);
  }
  final app = await pumpApp(
    tester,
    contractState('home'),
    brightness: brightness,
    devices: controller,
    section: 'this-pc',
  );
  return DevicesApp(app, channel, controller);
}
