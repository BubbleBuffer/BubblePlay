import 'dart:async';
import 'dart:convert';

import 'devices_channel.dart';

/// A [DevicesChannel] for tests: feed it host lines, read back what was sent.
class FakeDevicesChannel implements DevicesChannel {
  final StreamController<String> _lines = StreamController<String>.broadcast(sync: true);

  /// Commands the UI sent, in order.
  final List<Map<String, Object?>> sent = [];

  bool disposed = false;

  @override
  Stream<String> get lines => _lines.stream;

  /// Delivers one host message.
  void emit(Map<String, Object?> message) => _lines.add(jsonEncode(message));

  /// Delivers a raw line (to test malformed input).
  void emitRaw(String line) => _lines.add(line);

  @override
  void send(Map<String, Object?> command) => sent.add(command);

  @override
  void dispose() {
    disposed = true;
    _lines.close();
  }
}
