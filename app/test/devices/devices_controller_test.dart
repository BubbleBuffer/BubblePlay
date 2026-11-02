import 'package:bubbleplay_app/src/devices/devices_controller.dart';
import 'package:bubbleplay_app/src/devices/devices_model.dart';
import 'package:bubbleplay_app/src/devices/fake_devices_channel.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/devices_fixtures.dart';

void main() {
  late FakeDevicesChannel channel;
  late DevicesController controller;
  var clock = devicesNow;

  setUp(() {
    clock = devicesNow;
    channel = FakeDevicesChannel();
    controller = DevicesController(channel, now: () => clock);
  });
  tearDown(() => controller.dispose());

  DevicesState get() => controller.state;

  test('starts connecting with nothing known', () {
    expect(get().connection, isA<LinkConnecting>());
    expect(get().pcName, isNull);
    expect(get().devices, isEmpty);
    expect(get().pairing, isA<PairingNone>());
  });

  test('connected, then hello and list, fill the state in replay order', () {
    for (final m in connectedHost()) {
      channel.emit(m);
    }
    expect(get().isConnected, isTrue);
    expect(get().pcName, 'Atelier');
    expect(get().pcId, 'K7QX9M2PD4WH6R3T');
    expect(get().relayOrigin, 'https://relay.example');
    expect(get().relayOnline, isTrue);
    expect([for (final d in get().devices) d.name], ['Pixel 8', 'Surface Go', 'Old tablet']);
    expect(get().devices[0].connected, isTrue);
    expect(get().devices[2].lastSeen, isNull);
    expect(get().devices[1].lastSeen, devicesNow.subtract(const Duration(hours: 3)));
  });

  test('a replay with an open pairing lands in that phase', () {
    channel.emit({'type': 'connected'});
    channel.emit(helloMsg());
    channel.emit(listMsg());
    channel.emit(pairWaitingMsg());
    expect(get().pairing, isA<PairingWaiting>());
    channel.emit(pairRequestMsg());
    final phase = get().pairing as PairingRequest;
    expect(phase.id, 'dev-new');
    expect(phase.name, 'Galaxy Tab');
    expect(phase.sasDisplay, '482 913');
  });

  test('relay events change relayOnline', () {
    channel.emit(helloMsg());
    channel.emit({'type': 'relay', 'online': false});
    expect(get().relayOnline, isFalse);
    channel.emit({'type': 'relay', 'online': true});
    expect(get().relayOnline, isTrue);
  });

  test('pairWaiting carries code, qr, pc id and expiry', () {
    channel.emit(pairWaitingMsg());
    final phase = get().pairing as PairingWaiting;
    expect(phase.codeDisplay, '70Q1K-9MXR1');
    expect(phase.qrUri, startsWith('bubbleplay://pair?'));
    expect(phase.pcId, 'K7QX9M2PD4WH6R3T');
    expect(phase.expiresAt, devicesNow.add(const Duration(seconds: 272)));
  });

  test('countdown follows the injected clock and stops at zero', () {
    expect(controller.remaining, isNull);
    channel.emit(pairWaitingMsg());
    expect(controller.remaining, const Duration(seconds: 272));
    clock = devicesNow.add(const Duration(seconds: 100));
    expect(controller.remaining, const Duration(seconds: 172));
    clock = devicesNow.add(const Duration(seconds: 400));
    expect(controller.remaining, Duration.zero);
  });

  test('the ending pair events each set their phase', () {
    channel.emit({'type': 'pairDone', 'id': 'dev-new', 'name': 'Galaxy Tab'});
    expect((get().pairing as PairingDone).name, 'Galaxy Tab');
    channel.emit({'type': 'pairDenied'});
    expect(get().pairing, isA<PairingDenied>());
    channel.emit({'type': 'pairFailed'});
    expect(get().pairing, isA<PairingFailed>());
    channel.emit({'type': 'pairExpired'});
    expect(get().pairing, isA<PairingExpired>());
    channel.emit({'type': 'pairStopped'});
    expect(get().pairing, isA<PairingStopped>());
  });

  test('saveError is kept until dismissed', () {
    channel.emit({'type': 'saveError', 'message': 'Disk is full.'});
    expect(get().saveError, 'Disk is full.');
    controller.dismiss();
    expect(get().saveError, isNull);
  });

  test('show fires the stream and stays pending until consumed', () async {
    var shown = 0;
    final sub = controller.shows.listen((_) => shown++);
    channel.emit({'type': 'show'});
    await Future<void>.delayed(Duration.zero);
    expect(shown, 1);
    expect(controller.showPending, isTrue);
    controller.consumeShow();
    expect(controller.showPending, isFalse);
    await sub.cancel();
  });

  test('disconnected keeps the host name, drops pairing and records the reason', () {
    for (final m in connectedHost()) {
      channel.emit(m);
    }
    channel.emit(pairWaitingMsg());
    channel.emit({'type': 'disconnected', 'reason': 'notTrusted'});
    expect(get().isConnected, isFalse);
    expect((get().connection as LinkDisconnected).reason, DisconnectReason.notTrusted);
    expect(get().pairing, isA<PairingNone>());
    expect(get().pcName, 'Atelier');
  });

  test('every disconnect reason parses; an unknown one reads as io', () {
    for (final reason in DisconnectReason.values) {
      channel.emit({'type': 'disconnected', 'reason': reason.name});
      expect((get().connection as LinkDisconnected).reason, reason);
    }
    channel.emit({'type': 'disconnected', 'reason': 'somethingNew'});
    expect((get().connection as LinkDisconnected).reason, DisconnectReason.io);
  });

  test('a new connection starts from a clean slate', () {
    for (final m in connectedHost()) {
      channel.emit(m);
    }
    channel.emit(pairWaitingMsg());
    channel.emit({'type': 'saveError', 'message': 'x'});
    channel.emit({'type': 'disconnected', 'reason': 'io'});
    channel.emit({'type': 'connected'});
    expect(get().isConnected, isTrue);
    expect(get().devices, isEmpty);
    expect(get().pairing, isA<PairingNone>());
    expect(get().saveError, isNull);
  });

  test('unknown types and malformed lines are ignored', () {
    channel.emit(helloMsg());
    var notified = 0;
    controller.addListener(() => notified++);
    channel.emit({'type': 'somethingFromTheFuture', 'x': 1});
    channel.emitRaw('not json');
    channel.emitRaw('[1,2]');
    channel.emitRaw('{"type":"pairRequest","id":1}');
    channel.emitRaw('{"type":"hello"}');
    channel.emitRaw('{"noType":true}');
    expect(notified, 0);
    expect(get().pcName, 'Atelier');
  });

  test('commands go out as the pipe documents them', () {
    controller
      ..open()
      ..revoke('dev-a')
      ..startPairing()
      ..cancelPairing()
      ..allow('dev-new')
      ..deny('dev-new');
    expect(channel.sent, [
      {'type': 'open'},
      {'type': 'revoke', 'id': 'dev-a'},
      {'type': 'startPairing'},
      {'type': 'cancelPairing'},
      {'type': 'allow', 'id': 'dev-new'},
      {'type': 'deny', 'id': 'dev-new'},
    ]);
  });

  test('starting pairing closes an ended one', () {
    channel.emit({'type': 'pairDenied'});
    controller.startPairing();
    expect(get().pairing, isA<PairingNone>());
  });

  test('no secret appears in any toString', () {
    channel.emit(pairWaitingMsg());
    final waiting = get().toString() + get().pairing.toString();
    expect(waiting, isNot(contains('70Q1K')));
    expect(waiting, isNot(contains('bubbleplay://')));
    final lines = [pairWaitingMsg(), pairRequestMsg()];
    for (final line in lines) {
      final event = parseDevicesLine(
        '{"type":"${line['type']}",'
        '"codeDisplay":"70Q1K-9MXR1","qrUri":"bubbleplay://pair?x","pcId":"P","expiresAtMs":1,'
        '"id":"i","name":"n","sasDisplay":"482 913"}',
      );
      final text = event.toString();
      expect(text, isNot(contains('70Q1K')));
      expect(text, isNot(contains('bubbleplay://')));
      expect(text, isNot(contains('482')));
    }
    channel.emit(pairRequestMsg());
    expect(get().pairing.toString(), isNot(contains('482')));
  });

  test('dispose closes the channel and stops reacting', () {
    controller.dispose();
    expect(channel.disposed, isTrue);
    controller.apply(const ShowEvent());
    controller.open();
    expect(channel.sent, isEmpty);
  });
}
