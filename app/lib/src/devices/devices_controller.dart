import 'dart:async';

import 'package:flutter/foundation.dart';

import 'devices_channel.dart';
import 'devices_model.dart';

/// Where the link to the host stands.
sealed class DevicesConnection {
  const DevicesConnection();
}

class LinkConnecting extends DevicesConnection {
  const LinkConnecting();
}

class LinkConnected extends DevicesConnection {
  const LinkConnected();
}

class LinkDisconnected extends DevicesConnection {
  const LinkDisconnected(this.reason);
  final DisconnectReason reason;
}

/// What the pairing flow is doing. Code, QR URI and comparison number are held here for the UI
/// and never appear in `toString`.
sealed class PairingPhase {
  const PairingPhase();
}

class PairingNone extends PairingPhase {
  const PairingNone();
}

class PairingWaiting extends PairingPhase {
  const PairingWaiting({
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
  String toString() => 'PairingWaiting(expiresAt: $expiresAt, secrets hidden)';
}

/// A device asked to pair and waits for Allow or Deny.
class PairingRequest extends PairingPhase {
  const PairingRequest({required this.id, required this.name, required this.sasDisplay});
  final String id;
  final String name;
  final String sasDisplay;

  @override
  String toString() => 'PairingRequest(id: $id, name: $name, number hidden)';
}

class PairingDone extends PairingPhase {
  const PairingDone(this.name);
  final String name;
}

class PairingDenied extends PairingPhase {
  const PairingDenied();
}

class PairingFailed extends PairingPhase {
  const PairingFailed();
}

class PairingExpired extends PairingPhase {
  const PairingExpired();
}

class PairingStopped extends PairingPhase {
  const PairingStopped();
}

/// Everything the Devices UI shows, reduced from the host's messages.
@immutable
class DevicesState {
  const DevicesState({
    this.connection = const LinkConnecting(),
    this.pcName,
    this.pcId,
    this.relayOrigin,
    this.relayOnline = false,
    this.devices = const [],
    this.pairing = const PairingNone(),
    this.saveError,
  });

  final DevicesConnection connection;
  final String? pcName;
  final String? pcId;
  final String? relayOrigin;
  final bool relayOnline;
  final List<PairedDevice> devices;
  final PairingPhase pairing;
  final String? saveError;

  bool get isConnected => connection is LinkConnected;

  DevicesState copyWith({
    DevicesConnection? connection,
    String? pcName,
    String? pcId,
    String? relayOrigin,
    bool? relayOnline,
    List<PairedDevice>? devices,
    PairingPhase? pairing,
    Object? saveError = _keep,
  }) =>
      DevicesState(
        connection: connection ?? this.connection,
        pcName: pcName ?? this.pcName,
        pcId: pcId ?? this.pcId,
        relayOrigin: relayOrigin ?? this.relayOrigin,
        relayOnline: relayOnline ?? this.relayOnline,
        devices: devices ?? this.devices,
        pairing: pairing ?? this.pairing,
        saveError: identical(saveError, _keep) ? this.saveError : saveError as String?,
      );

  @override
  String toString() =>
      'DevicesState($connection, devices: ${devices.length}, pairing: $pairing)';
}

const Object _keep = Object();

/// Reduces the host's lines to [DevicesState] and sends the commands. Reconnecting is the
/// channel's job; this only reflects `connected` and `disconnected`.
class DevicesController extends ChangeNotifier {
  DevicesController(this._channel, {DateTime Function()? now})
      : _now = now ?? DateTime.now {
    _subscription = _channel.lines.listen(_onLine);
  }

  final DevicesChannel _channel;
  final DateTime Function() _now;
  late final StreamSubscription<String> _subscription;
  final StreamController<void> _shows = StreamController<void>.broadcast();
  DevicesState _state = const DevicesState();
  bool _showPending = false;
  bool _disposed = false;

  DevicesState get state => _state;

  /// The current time by this controller's clock (the countdown and "last seen" use it).
  DateTime get now => _now();

  /// Fires when the host asked to show the Devices section.
  Stream<void> get shows => _shows.stream;

  /// True when a `show` arrived that nobody has acted on yet (the UI was not up).
  bool get showPending => _showPending;

  void consumeShow() => _showPending = false;

  /// Time left on the pairing code; null unless pairing is waiting. Never negative.
  Duration? get remaining {
    final phase = _state.pairing;
    if (phase is! PairingWaiting) return null;
    final left = phase.expiresAt.difference(_now());
    return left.isNegative ? Duration.zero : left;
  }

  void _onLine(String line) {
    final event = parseDevicesLine(line);
    if (event != null) apply(event);
  }

  /// Reduces one event. Public so tests can drive it without a channel.
  void apply(DevicesEvent event) {
    if (_disposed) return;
    switch (event) {
      case ConnectedEvent():
        _state = DevicesState(connection: const LinkConnected(), pcName: _state.pcName, pcId: _state.pcId);
      case DisconnectedEvent(:final reason):
        _state = _state.copyWith(
          connection: LinkDisconnected(reason),
          pairing: const PairingNone(),
          relayOnline: false,
        );
      case HelloEvent():
        _state = _state.copyWith(
          pcName: event.pcName,
          pcId: event.pcId,
          relayOrigin: event.relayOrigin,
          relayOnline: event.relayOnline,
        );
      case RelayEvent(:final online):
        _state = _state.copyWith(relayOnline: online);
      case ListEvent(:final devices):
        _state = _state.copyWith(devices: devices);
      case PairWaitingEvent():
        _state = _state.copyWith(
          pairing: PairingWaiting(
            codeDisplay: event.codeDisplay,
            qrUri: event.qrUri,
            pcId: event.pcId,
            expiresAt: event.expiresAt,
          ),
        );
      case PairRequestEvent():
        _state = _state.copyWith(
          pairing: PairingRequest(id: event.id, name: event.name, sasDisplay: event.sasDisplay),
        );
      case PairDoneEvent(:final name):
        _state = _state.copyWith(pairing: PairingDone(name));
      case PairDeniedEvent():
        _state = _state.copyWith(pairing: const PairingDenied());
      case PairFailedEvent():
        _state = _state.copyWith(pairing: const PairingFailed());
      case PairExpiredEvent():
        _state = _state.copyWith(pairing: const PairingExpired());
      case PairStoppedEvent():
        _state = _state.copyWith(pairing: const PairingStopped());
      case SaveErrorEvent(:final message):
        _state = _state.copyWith(saveError: message);
      case ShowEvent():
        _showPending = true;
        _shows.add(null);
        return;
    }
    notifyListeners();
  }

  /// Asks the host for a fresh list.
  void open() => _send({'type': 'open'});

  void revoke(String id) => _send({'type': 'revoke', 'id': id});

  void startPairing() {
    _clearEnded();
    _send({'type': 'startPairing'});
  }

  void cancelPairing() => _send({'type': 'cancelPairing'});

  void allow(String id) => _send({'type': 'allow', 'id': id});

  void deny(String id) => _send({'type': 'deny', 'id': id});

  /// Closes an ended pairing (done, denied, failed, expired, stopped) and the save error. Local
  /// only; nothing is sent.
  void dismiss() {
    _state = _state.copyWith(pairing: const PairingNone(), saveError: null);
    notifyListeners();
  }

  void _clearEnded() {
    if (_state.pairing is PairingNone && _state.saveError == null) return;
    _state = _state.copyWith(pairing: const PairingNone(), saveError: null);
    notifyListeners();
  }

  void _send(Map<String, Object?> command) {
    if (_disposed) return;
    _channel.send(command);
  }

  @override
  void dispose() {
    if (_disposed) return;
    _disposed = true;
    _subscription.cancel();
    _shows.close();
    _channel.dispose();
    super.dispose();
  }
}
