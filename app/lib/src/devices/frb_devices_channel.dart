// ignore_for_file: prefer_initializing_formals
import 'dart:async';
import 'dart:convert';

import '../rust/api.dart' as api;
import 'devices_channel.dart';

/// Windows: the host's devices pipe through flutter_rust_bridge. A thin adapter over the three
/// generated functions; reconnecting and the synthetic `connected`/`disconnected` lines are done
/// on the Rust side.
class FrbDevicesChannel implements DevicesChannel {
  FrbDevicesChannel({
    required Stream<String> Function() connect,
    required void Function(String line) sendLine,
    required void Function() disconnect,
  })  : _sendLine = sendLine,
        _disconnect = disconnect,
        lines = connect();

  /// Opens the channel over the generated binding (RustLib must be initialised). Returns null
  /// if that fails, so the app runs without a Devices UI.
  static FrbDevicesChannel? tryOpen() {
    try {
      return FrbDevicesChannel(
        connect: api.devicesConnect,
        sendLine: (line) => api.devicesSend(line: line),
        disconnect: () => unawaited(api.devicesDisconnect()),
      );
    } catch (_) {
      return null;
    }
  }

  final void Function(String line) _sendLine;
  final void Function() _disconnect;

  @override
  final Stream<String> lines;

  @override
  void send(Map<String, Object?> command) => _sendLine(jsonEncode(command));

  @override
  void dispose() => _disconnect();
}
