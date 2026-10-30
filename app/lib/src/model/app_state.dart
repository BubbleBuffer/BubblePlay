import 'common.dart';
import 'flow.dart';
import 'host.dart';
import 'json_util.dart';
import 'session.dart';

/// The one snapshot the UI reads each frame.
class AppState {
  const AppState({
    required this.thisDevice,
    required this.pcs,
    this.host,
    this.flow,
    this.session,
  });

  factory AppState.fromJson(Object? json) {
    final map = jsonMap(json);
    final host = map['host'];
    final flow = map['flow'];
    final session = map['session'];
    return AppState(
      thisDevice: ThisDevice.fromJson(jsonField(map, 'this_device')),
      pcs: jsonListOf(jsonField(map, 'pcs'), RemotePc.fromJson),
      host: host == null ? null : HostStatus.fromJson(host),
      flow: flow == null ? null : Flow.fromJson(flow),
      session: session == null ? null : Session.fromJson(session),
    );
  }

  final ThisDevice thisDevice;

  /// Remote PCs paired with this device's key ("Your PCs").
  final List<RemotePc> pcs;

  /// The BubblePlay host on this computer ("This PC"), when one is installed.
  final HostStatus? host;

  /// The connect / pair / rotate flow, when one is in progress.
  final Flow? flow;

  /// The live session, when connected (or reconnecting).
  final Session? session;

  Object? toJson() => <String, Object?>{
        'this_device': thisDevice.toJson(),
        'pcs': [for (final pc in pcs) pc.toJson()],
        'host': host?.toJson(),
        'flow': flow?.toJson(),
        'session': session?.toJson(),
      };
}

/// This computer's BubblePlay device identity.
class ThisDevice {
  const ThisDevice({required this.id, required this.keyPath});

  factory ThisDevice.fromJson(Object? json) {
    final map = jsonMap(json);
    return ThisDevice(
      id: jsonString(jsonField(map, 'id')),
      keyPath: jsonString(jsonField(map, 'key_path')),
    );
  }

  /// Device ID, 16 Crockford base32 chars shown in four groups.
  final String id;

  /// Where the key file lives, as shown in Settings.
  final String keyPath;

  Object? toJson() => <String, Object?>{'id': id, 'key_path': keyPath};
}

/// One of "Your PCs".
class RemotePc {
  const RemotePc({
    required this.id,
    required this.name,
    required this.kind,
    required this.relay,
    required this.online,
    this.lastMode,
    this.lastSeen,
    required this.allowed,
    this.vouchRequest,
    this.problem,
    this.defaultMonitor,
  });

  factory RemotePc.fromJson(Object? json) {
    final map = jsonMap(json);
    final lastMode = map['last_mode'];
    final vouchRequest = map['vouch_request'];
    final problem = map['problem'];
    return RemotePc(
      id: jsonString(jsonField(map, 'id')),
      name: jsonString(jsonField(map, 'name')),
      kind: PcKind.fromJson(jsonField(map, 'kind')),
      relay: jsonString(jsonField(map, 'relay')),
      online: jsonBool(jsonField(map, 'online')),
      lastMode: lastMode == null ? null : StreamMode.fromJson(lastMode),
      lastSeen: jsonStringOrNull(map['last_seen']),
      allowed: jsonString(jsonField(map, 'allowed')),
      vouchRequest:
          vouchRequest == null ? null : VouchRequest.fromJson(vouchRequest),
      problem: problem == null ? null : PcProblem.fromJson(problem),
      defaultMonitor: jsonStringOrNull(map['default_monitor']),
    );
  }

  /// PC ID (rooms are keyed by it).
  final String id;
  final String name;
  final PcKind kind;
  final String relay;
  final bool online;

  /// None for a PC never connected to; the UI then says Work.
  final StreamMode? lastMode;

  /// Display text, e.g. "Today 09:12" or "2 Oct".
  final String? lastSeen;

  /// Display text of what this device may do there, e.g. "View and input".
  final String allowed;

  /// A pairing request for this PC that this device could vouch for.
  final VouchRequest? vouchRequest;

  /// The last known trouble with this PC, shown on its row and detail panel.
  final PcProblem? problem;

  /// The monitor selector this PC opens on; None means the host's default.
  final String? defaultMonitor;

  Object? toJson() => <String, Object?>{
        'id': id,
        'name': name,
        'kind': kind.toJson(),
        'relay': relay,
        'online': online,
        'last_mode': lastMode?.toJson(),
        'last_seen': lastSeen,
        'allowed': allowed,
        'vouch_request': vouchRequest?.toJson(),
        'problem': problem?.toJson(),
        'default_monitor': defaultMonitor,
      };
}

/// Trouble with a saved PC that Your PCs shows before anyone clicks Connect.
enum PcProblem {
  /// The PC's relay did not answer, so its online state is unknown.
  relayUnreachable('RelayUnreachable'),

  /// The last connect was refused: this device's key is not on the PC's list.
  notAccepted('NotAccepted'),

  /// The PC answered with a key that does not match the pinned one.
  keyChanged('KeyChanged');

  const PcProblem(this.wire);

  /// The name serde writes.
  final String wire;

  static PcProblem fromJson(Object? json) {
    for (final value in values) {
      if (value.wire == json) return value;
    }
    throw FormatException('Unknown PcProblem: $json');
  }

  String toJson() => wire;
}

/// A pending pairing request on a saved PC that this device may vouch for.
class VouchRequest {
  const VouchRequest({
    required this.requestId,
    required this.deviceName,
    required this.deviceId,
    required this.number,
    required this.expiresInSecs,
    required this.permissions,
  });

  factory VouchRequest.fromJson(Object? json) {
    final map = jsonMap(json);
    return VouchRequest(
      requestId: jsonString(jsonField(map, 'request_id')),
      deviceName: jsonString(jsonField(map, 'device_name')),
      deviceId: jsonString(jsonField(map, 'device_id')),
      number: jsonInt(jsonField(map, 'number')),
      expiresInSecs: jsonInt(jsonField(map, 'expires_in_secs')),
      permissions:
          jsonListOf(jsonField(map, 'permissions'), Permission.fromJson),
    );
  }

  final String requestId;
  final String deviceName;
  final String deviceId;

  /// The 6-digit comparison number.
  final int number;
  final int expiresInSecs;

  /// Permissions the vouch would grant; "Before sign-in" is never offered.
  final List<Permission> permissions;

  Object? toJson() => <String, Object?>{
        'request_id': requestId,
        'device_name': deviceName,
        'device_id': deviceId,
        'number': number,
        'expires_in_secs': expiresInSecs,
        'permissions': [for (final permission in permissions) permission.toJson()],
      };
}

/// One permission a request would grant.
class Permission {
  const Permission({
    required this.key,
    required this.label,
    required this.granted,
  });

  factory Permission.fromJson(Object? json) {
    final map = jsonMap(json);
    return Permission(
      key: jsonString(jsonField(map, 'key')),
      label: jsonString(jsonField(map, 'label')),
      granted: jsonBool(jsonField(map, 'granted')),
    );
  }

  final String key;
  final String label;
  final bool granted;

  Object? toJson() =>
      <String, Object?>{'key': key, 'label': label, 'granted': granted};
}
