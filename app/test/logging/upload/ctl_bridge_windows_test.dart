import 'dart:async';
import 'dart:convert';

import 'package:bubbleplay_app/src/logging/upload/ctl_bridge.dart';
import 'package:bubbleplay_app/src/logging/upload/ctl_bridge_windows.dart';
import 'package:bubbleplay_app/src/rust/api.dart' show CtlLogState;
import 'package:flutter_test/flutter_test.dart';

const _maxInt64 = 9223372036854775807;

class _FakeApi {
  final StreamController<String> lines = StreamController<String>();
  final List<String> sent = [];
  bool accept = true;
  bool throwOnSend = false;
  int stateCalls = 0;
  CtlLogState state = const CtlLogState(
    open: true,
    bufferedBytes: 0,
    sinceLastInputMs: _maxInt64,
  );

  WindowsCtlBridge bridge() => WindowsCtlBridge(
        events: () => lines.stream,
        send: (json) {
          if (throwOnSend) throw StateError('boom');
          sent.add(json);
          return accept;
        },
        state: () {
          stateCalls++;
          return state;
        },
        pollInterval: const Duration(milliseconds: 20),
      );

  void push(Map<String, Object?> event) => lines.add(jsonEncode(event));
}

Map<String, Object?> _opened() => {
      'kind': 'opened',
      'pcId': 'pc-1',
      'peerCaps': ['client-logs-v1', 'other'],
    };

void main() {
  late _FakeApi api;
  late WindowsCtlBridge bridge;
  late List<CtlEvent> events;

  setUp(() {
    api = _FakeApi();
    bridge = api.bridge();
    events = [];
    bridge.events.listen(events.add);
  });

  tearDown(() => bridge.dispose());

  Future<void> settle([int ms = 5]) =>
      Future<void>.delayed(Duration(milliseconds: ms));

  test('events decode: opened, message, closed', () async {
    api.push(_opened());
    api.push({
      'kind': 'message',
      'json': {'type': 'logs-state', 'cursors': {}},
    });
    api.push({'kind': 'closed'});
    await settle();
    expect(events, hasLength(3));
    final opened = events[0] as CtlOpened;
    expect(opened.pcId, 'pc-1');
    expect(opened.peerCaps, {'client-logs-v1', 'other'});
    expect((events[1] as CtlMessage).json['type'], 'logs-state');
    expect(events[2], isA<CtlClosed>());
  });

  test('malformed and out-of-connection events are dropped', () async {
    api.push({
      'kind': 'message',
      'json': {'type': 'logs-state'},
    }); // before opened
    api.lines.add('not json');
    api.lines.add('[]');
    api.push({'kind': 'mystery'});
    api.push({'kind': 'closed'});
    await settle();
    expect(events, isEmpty);

    api.push(_opened());
    api.push({'kind': 'message', 'json': 'not an object'});
    await settle();
    expect(events, hasLength(1), reason: 'only the opened event');
  });

  test('send is false when closed, and sends JSON when open', () async {
    expect(await bridge.send({'type': 'logs-batch'}), isFalse);
    expect(api.sent, isEmpty);

    api.push(_opened());
    await settle();
    expect(await bridge.send({'type': 'logs-batch', 'id': 1}), isTrue);
    expect(jsonDecode(api.sent.single), {'type': 'logs-batch', 'id': 1});

    api.accept = false;
    expect(await bridge.send({'type': 'logs-batch'}), isFalse);
    api.throwOnSend = true;
    expect(await bridge.send({'type': 'logs-batch'}), isFalse);

    api.push({'kind': 'closed'});
    await settle();
    api.accept = true;
    api.throwOnSend = false;
    final before = api.sent.length;
    expect(await bridge.send({'type': 'logs-batch'}), isFalse);
    expect(api.sent.length, before);
  });

  test('state is cached from the core and polled only while open', () async {
    await settle(80);
    expect(api.stateCalls, 0, reason: 'no polling without a connection');
    expect(bridge.bufferedBytes, 0);
    expect(bridge.sinceLastInput, const Duration(hours: 1));

    api.state = const CtlLogState(
      open: true,
      bufferedBytes: 4096,
      sinceLastInputMs: 1000,
    );
    api.push(_opened());
    await settle(80);
    expect(api.stateCalls, greaterThan(1));
    expect(bridge.bufferedBytes, 4096);
    expect(
      bridge.sinceLastInput,
      greaterThanOrEqualTo(const Duration(seconds: 1)),
    );
    expect(bridge.sinceLastInput, lessThan(const Duration(seconds: 2)));

    api.push({'kind': 'closed'});
    await settle();
    final calls = api.stateCalls;
    await settle(80);
    expect(api.stateCalls, calls, reason: 'polling stops when closed');
    expect(bridge.bufferedBytes, 0);
  });

  test('no input yet from the core reads as a long time ago', () async {
    api.push(_opened());
    await settle(50);
    expect(bridge.sinceLastInput, const Duration(hours: 1));
  });

  test('a state that says closed ends the connection once', () async {
    api.push(_opened());
    await settle(30);
    api.state = const CtlLogState(
      open: false,
      bufferedBytes: 0,
      sinceLastInputMs: 0,
    );
    await settle(80);
    expect(events.whereType<CtlClosed>(), hasLength(1));
    expect(await bridge.send({'type': 'logs-batch'}), isFalse);
  });

  test('a reconnect gives a new opened', () async {
    api.push(_opened());
    api.push({'kind': 'closed'});
    api.push(_opened());
    await settle();
    expect(events.map((e) => e.runtimeType), [
      CtlOpened,
      CtlClosed,
      CtlOpened,
    ]);
  });

  test('createWindowsCtlBridge returns null when the core is not loaded', () {
    expect(createWindowsCtlBridge(), isNull);
  });
}
