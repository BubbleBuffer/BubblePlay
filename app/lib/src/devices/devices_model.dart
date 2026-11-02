import 'dart:convert';

import '../logging/app_log.dart';

/// Why the host link is down (the synthetic `disconnected` line's `reason`).
enum DisconnectReason {
  hostNotRunning,
  notTrusted,
  protocol,
  io,
  unsupported;

  static DisconnectReason fromWire(Object? value) {
    for (final reason in values) {
      if (reason.name == value) return reason;
    }
    return DisconnectReason.io;
  }
}

/// One paired device.
class PairedDevice {
  const PairedDevice({
    required this.id,
    required this.name,
    this.lastSeen,
    required this.connected,
  });

  final String id;
  final String name;
  final DateTime? lastSeen;
  final bool connected;
}

/// A message from the host (or the bridge), parsed. `toString` of every event that carries a
/// secret (code, QR URI, comparison number) hides it.
sealed class DevicesEvent {
  const DevicesEvent();
}

class ConnectedEvent extends DevicesEvent {
  const ConnectedEvent();
}

class DisconnectedEvent extends DevicesEvent {
  const DisconnectedEvent(this.reason);
  final DisconnectReason reason;
}

class HelloEvent extends DevicesEvent {
  const HelloEvent({
    required this.pcName,
    required this.pcId,
    required this.relayOrigin,
    required this.relayOnline,
  });
  final String pcName;
  final String pcId;
  final String relayOrigin;
  final bool relayOnline;
}

class RelayEvent extends DevicesEvent {
  const RelayEvent(this.online);
  final bool online;
}

class ListEvent extends DevicesEvent {
  const ListEvent(this.devices);
  final List<PairedDevice> devices;
}

class PairWaitingEvent extends DevicesEvent {
  const PairWaitingEvent({
    required this.codeDisplay,
    required this.qrUri,
    required this.pcId,
    required this.expiresAt,
  });
  final String codeDisplay;
  final String qrUri;
  final String pcId;
  final DateTime expiresAt;

  @override
  String toString() => 'PairWaitingEvent(pcId: $pcId, expiresAt: $expiresAt, secrets hidden)';
}

class PairExpiredEvent extends DevicesEvent {
  const PairExpiredEvent();
}

class PairStoppedEvent extends DevicesEvent {
  const PairStoppedEvent();
}

class PairFailedEvent extends DevicesEvent {
  const PairFailedEvent();
}

class PairDeniedEvent extends DevicesEvent {
  const PairDeniedEvent();
}

class PairRequestEvent extends DevicesEvent {
  const PairRequestEvent({required this.id, required this.name, required this.sasDisplay});
  final String id;
  final String name;
  final String sasDisplay;

  @override
  String toString() => 'PairRequestEvent(id: $id, name: $name, number hidden)';
}

class PairDoneEvent extends DevicesEvent {
  const PairDoneEvent({required this.id, required this.name});
  final String id;
  final String name;
}

class SaveErrorEvent extends DevicesEvent {
  const SaveErrorEvent(this.message);
  final String message;
}

class ShowEvent extends DevicesEvent {
  const ShowEvent();
}

/// Parses one line. Returns null for a malformed line or an unknown `type` (both ignored).
DevicesEvent? parseDevicesLine(String line) {
  try {
    final json = jsonDecode(line);
    if (json is! Map) return null;
    return _parse(json.cast<String, Object?>());
  } catch (e) {
    // Only the error class: a format error's message quotes the line.
    AppLog.warn('Devices', 'Ignored a malformed line from the host', fields: {'error_type': e.runtimeType.toString()});
    return null;
  }
}

String _string(Map<String, Object?> m, String key) {
  final v = m[key];
  if (v is String) return v;
  throw FormatException('missing $key');
}

DevicesEvent? _parse(Map<String, Object?> m) => switch (m['type']) {
      'connected' => const ConnectedEvent(),
      'disconnected' => DisconnectedEvent(DisconnectReason.fromWire(m['reason'])),
      'hello' => HelloEvent(
          pcName: _string(m, 'pcName'),
          pcId: _string(m, 'pcId'),
          relayOrigin: m['relayOrigin'] as String? ?? '',
          relayOnline: m['relayOnline'] == true,
        ),
      'relay' => RelayEvent(m['online'] == true),
      'list' => ListEvent([
          for (final d in (m['devices'] as List? ?? const []))
            _device((d as Map).cast<String, Object?>()),
        ]),
      'pairWaiting' => PairWaitingEvent(
          codeDisplay: _string(m, 'codeDisplay'),
          qrUri: _string(m, 'qrUri'),
          pcId: _string(m, 'pcId'),
          expiresAt: DateTime.fromMillisecondsSinceEpoch(m['expiresAtMs'] as int, isUtc: true),
        ),
      'pairExpired' => const PairExpiredEvent(),
      'pairStopped' => const PairStoppedEvent(),
      'pairFailed' => const PairFailedEvent(),
      'pairDenied' => const PairDeniedEvent(),
      'pairRequest' => PairRequestEvent(
          id: _string(m, 'id'),
          name: _string(m, 'name'),
          sasDisplay: _string(m, 'sasDisplay'),
        ),
      'pairDone' => PairDoneEvent(id: _string(m, 'id'), name: _string(m, 'name')),
      'saveError' => SaveErrorEvent(_string(m, 'message')),
      'show' => const ShowEvent(),
      _ => null,
    };

PairedDevice _device(Map<String, Object?> m) {
  final seen = m['lastSeen'];
  return PairedDevice(
    id: _string(m, 'id'),
    name: _string(m, 'name'),
    lastSeen: seen is String ? DateTime.tryParse(seen) : null,
    connected: m['connected'] == true,
  );
}
