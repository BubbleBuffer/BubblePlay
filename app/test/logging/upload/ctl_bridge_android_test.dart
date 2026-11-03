import 'dart:convert';

import 'package:bubbleplay_app/src/backend/pigeon.g.dart';
import 'package:bubbleplay_app/src/logging/upload/ctl_bridge.dart';
import 'package:bubbleplay_app/src/logging/upload/ctl_bridge_android.dart';
import 'package:flutter_test/flutter_test.dart';

class _FakeHost extends CtlHostApi {
  final List<String> sent = [];
  bool accept = true;
  bool throwOnSend = false;
  int stateCalls = 0;
  CtlState state = CtlState(
    open: true,
    bufferedBytes: 0,
    sinceLastInputMs: 9223372036854775807,
    pcId: 'pc-1',
    peerCaps: ['client-logs-v1'],
  );

  @override
  Future<bool> ctlSend(String json) async {
    if (throwOnSend) throw StateError('boom');
    sent.add(json);
    return accept;
  }

  @override
  Future<CtlState> ctlState() async {
    stateCalls++;
    return state;
  }
}

const _opened = '{"pcId":"pc-1","peerCaps":["client-logs-v1","other"]}';

void main() {
  late _FakeHost host;
  late AndroidCtlBridge bridge;
  late List<CtlEvent> events;

  setUp(() {
    host = _FakeHost();
    bridge = AndroidCtlBridge(
      host: host,
      pollInterval: const Duration(milliseconds: 20),
    );
    events = [];
    bridge.events.listen(events.add);
  });

  tearDown(() => bridge.dispose());

  Future<void> settle([int ms = 5]) =>
      Future<void>.delayed(Duration(milliseconds: ms));

  test('events map: opened, message, closed', () async {
    bridge.receiver.ctlEvent('opened', _opened);
    bridge.receiver.ctlEvent('message', '{"type":"logs-state","cursors":{}}');
    bridge.receiver.ctlEvent('closed', '{}');
    await settle();
    expect(events, hasLength(3));
    final opened = events[0] as CtlOpened;
    expect(opened.pcId, 'pc-1');
    expect(opened.peerCaps, {'client-logs-v1', 'other'});
    expect((events[1] as CtlMessage).json['type'], 'logs-state');
    expect(events[2], isA<CtlClosed>());
  });

  test('malformed and out-of-connection events are dropped', () async {
    bridge.receiver.ctlEvent('message', '{"type":"logs-state"}'); // before opened
    bridge.receiver.ctlEvent('opened', 'not json');
    bridge.receiver.ctlEvent('mystery', '{}');
    bridge.receiver.ctlEvent('closed', '{}');
    await settle();
    expect(events, isEmpty);
  });

  test('send is false when closed, and sends JSON when open', () async {
    expect(await bridge.send({'type': 'logs'}), isFalse);
    expect(host.sent, isEmpty);

    bridge.receiver.ctlEvent('opened', _opened);
    expect(await bridge.send({'type': 'logs', 'id': 1}), isTrue);
    expect(jsonDecode(host.sent.single), {'type': 'logs', 'id': 1});

    host.accept = false;
    expect(await bridge.send({'type': 'logs'}), isFalse);
    host.throwOnSend = true;
    expect(await bridge.send({'type': 'logs'}), isFalse);

    bridge.receiver.ctlEvent('closed', '{}');
    host.accept = true;
    host.throwOnSend = false;
    final before = host.sent.length;
    expect(await bridge.send({'type': 'logs'}), isFalse);
    expect(host.sent.length, before);
  });

  test('state is cached from ctlState and polled only while open', () async {
    await settle(80);
    expect(host.stateCalls, 0, reason: 'no polling without a connection');
    expect(bridge.bufferedBytes, 0);

    host.state = CtlState(
      open: true,
      bufferedBytes: 4096,
      sinceLastInputMs: 1000,
      pcId: 'pc-1',
      peerCaps: ['client-logs-v1'],
    );
    bridge.receiver.ctlEvent('opened', _opened);
    await settle(80);
    expect(host.stateCalls, greaterThan(1));
    expect(bridge.bufferedBytes, 4096);
    expect(
      bridge.sinceLastInput,
      greaterThanOrEqualTo(const Duration(seconds: 1)),
    );
    expect(bridge.sinceLastInput, lessThan(const Duration(seconds: 2)));

    host.state.bufferedBytes = 10;
    await settle(80);
    expect(bridge.bufferedBytes, 10);

    bridge.receiver.ctlEvent('closed', '{}');
    await settle();
    final calls = host.stateCalls;
    await settle(80);
    expect(
      host.stateCalls,
      calls,
      reason: 'polling stops when the connection closes',
    );
    expect(bridge.bufferedBytes, 0);
  });

  test('no input yet reads as a long time ago, not an overflow', () async {
    bridge.receiver.ctlEvent('opened', _opened);
    await settle(40);
    expect(
      bridge.sinceLastInput,
      greaterThanOrEqualTo(const Duration(minutes: 30)),
    );
  });

  test('a state that says closed ends the connection', () async {
    bridge.receiver.ctlEvent('opened', _opened);
    host.state = CtlState(
      open: false,
      bufferedBytes: 0,
      sinceLastInputMs: 0,
      pcId: '',
      peerCaps: [],
    );
    await settle(60);
    expect(events.last, isA<CtlClosed>());
    expect(await bridge.send({'type': 'x'}), isFalse);
  });
}
