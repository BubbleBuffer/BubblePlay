import 'json_util.dart';

/// The BubblePlay host on this computer ("This PC").
class HostStatus {
  const HostStatus({
    required this.pcName,
    this.pcId,
    required this.run,
    this.viewer,
    required this.settings,
    required this.build,
    required this.canRollBack,
    required this.paired,
    this.request,
    this.answered,
  });

  factory HostStatus.fromJson(Object? json) {
    final map = jsonMap(json);
    final pcId = map['pc_id'];
    final viewer = map['viewer'];
    final request = map['request'];
    final answered = map['answered'];
    return HostStatus(
      pcName: jsonString(jsonField(map, 'pc_name')),
      pcId: jsonStringOrNull(pcId),
      run: HostRun.fromJson(jsonField(map, 'run')),
      viewer: viewer == null ? null : ConnectedViewer.fromJson(viewer),
      settings: HostSettings.fromJson(jsonField(map, 'settings')),
      build: BuildInfo.fromJson(jsonField(map, 'build')),
      canRollBack: jsonBool(jsonField(map, 'can_roll_back')),
      paired: jsonListOf(jsonField(map, 'paired'), PairedDevice.fromJson),
      request: request == null ? null : PairingRequest.fromJson(request),
      answered: answered == null ? null : AnsweredRequest.fromJson(answered),
    );
  }

  final String pcName;

  /// None until device keys land on the host.
  final String? pcId;
  final HostRun run;

  /// The viewer currently connected to this PC.
  final ConnectedViewer? viewer;
  final HostSettings settings;
  final BuildInfo build;
  final bool canRollBack;
  final List<PairedDevice> paired;

  /// An incoming pairing request waiting for Allow or Deny.
  final PairingRequest? request;

  /// A request answered elsewhere (tray or another device).
  final AnsweredRequest? answered;

  Object? toJson() => <String, Object?>{
        'pc_name': pcName,
        'pc_id': pcId,
        'run': run.toJson(),
        'viewer': viewer?.toJson(),
        'settings': settings.toJson(),
        'build': build.toJson(),
        'can_roll_back': canRollBack,
        'paired': [for (final device in paired) device.toJson()],
        'request': request?.toJson(),
        'answered': answered?.toJson(),
      };
}

/// Where the host program on this computer stands.
sealed class HostRun {
  const HostRun();

  factory HostRun.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'Starting':
        return const HostStarting();
      case 'Running':
        return const HostRunning();
      case 'Restarting':
        {
          final map = jsonMap(variant.value);
          return HostRestarting(inSecs: jsonInt(jsonField(map, 'in_secs')));
        }
      case 'Stopped':
        return const HostStopped();
      case 'NotConfigured':
        return const HostNotConfigured();
    }
    throw FormatException('Unknown HostRun variant: ${variant.name}');
  }

  Object? toJson();
}

/// The host is on its way up.
class HostStarting extends HostRun {
  const HostStarting();

  @override
  Object? toJson() => 'Starting';
}

/// The host is up.
class HostRunning extends HostRun {
  const HostRunning();

  @override
  Object? toJson() => 'Running';
}

/// The host is being restarted, in [inSecs] seconds.
class HostRestarting extends HostRun {
  const HostRestarting({required this.inSecs});

  final int inSecs;

  @override
  Object? toJson() => <String, Object?>{
        'Restarting': <String, Object?>{'in_secs': inSecs},
      };
}

/// The host is installed but not running.
class HostStopped extends HostRun {
  const HostStopped();

  @override
  Object? toJson() => 'Stopped';
}

/// No host is installed here.
class HostNotConfigured extends HostRun {
  const HostNotConfigured();

  @override
  Object? toJson() => 'NotConfigured';
}

/// The viewer currently connected to this PC.
class ConnectedViewer {
  const ConnectedViewer({required this.name, required this.since});

  factory ConnectedViewer.fromJson(Object? json) {
    final map = jsonMap(json);
    return ConnectedViewer(
      name: jsonString(jsonField(map, 'name')),
      since: jsonString(jsonField(map, 'since')),
    );
  }

  final String name;

  /// Display text, e.g. "09:12".
  final String since;

  Object? toJson() => <String, Object?>{'name': name, 'since': since};
}

/// The host's four on/off settings.
class HostSettings {
  const HostSettings({
    required this.streamSound,
    required this.adminMode,
    required this.startAtLogin,
    required this.cpuMode,
  });

  factory HostSettings.fromJson(Object? json) {
    final map = jsonMap(json);
    return HostSettings(
      streamSound: jsonBool(jsonField(map, 'stream_sound')),
      adminMode: jsonBool(jsonField(map, 'admin_mode')),
      startAtLogin: jsonBool(jsonField(map, 'start_at_login')),
      cpuMode: jsonBool(jsonField(map, 'cpu_mode')),
    );
  }

  final bool streamSound;
  final bool adminMode;
  final bool startAtLogin;

  /// Takes effect after a host restart.
  final bool cpuMode;

  Object? toJson() => <String, Object?>{
        'stream_sound': streamSound,
        'admin_mode': adminMode,
        'start_at_login': startAtLogin,
        'cpu_mode': cpuMode,
      };
}

/// One host setting as the UI toggles it.
sealed class HostSetting {
  const HostSetting();

  factory HostSetting.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'StreamSound':
        return StreamSound(jsonBool(variant.value));
      case 'AdminMode':
        return AdminMode(jsonBool(variant.value));
      case 'StartAtLogin':
        return StartAtLogin(jsonBool(variant.value));
      case 'CpuMode':
        return CpuMode(jsonBool(variant.value));
    }
    throw FormatException('Unknown HostSetting variant: ${variant.name}');
  }

  /// The value the setting is set to.
  bool get value;

  Object? toJson();
}

/// Whether the viewer's sound is played on this PC.
class StreamSound extends HostSetting {
  const StreamSound(this.value);

  @override
  final bool value;

  @override
  Object? toJson() => <String, Object?>{'StreamSound': value};
}

/// Whether input may reach elevated windows.
class AdminMode extends HostSetting {
  const AdminMode(this.value);

  @override
  final bool value;

  @override
  Object? toJson() => <String, Object?>{'AdminMode': value};
}

/// Whether the host starts when the user signs in.
class StartAtLogin extends HostSetting {
  const StartAtLogin(this.value);

  @override
  final bool value;

  @override
  Object? toJson() => <String, Object?>{'StartAtLogin': value};
}

/// Whether the host encodes on the CPU instead of the GPU.
class CpuMode extends HostSetting {
  const CpuMode(this.value);

  @override
  final bool value;

  @override
  Object? toJson() => <String, Object?>{'CpuMode': value};
}

/// Which host build is installed and whether it has proven itself.
class BuildInfo {
  const BuildInfo({required this.hash, required this.state});

  factory BuildInfo.fromJson(Object? json) {
    final map = jsonMap(json);
    return BuildInfo(
      hash: jsonString(jsonField(map, 'hash')),
      state: BuildState.fromJson(jsonField(map, 'state')),
    );
  }

  final String hash;
  final BuildState state;

  Object? toJson() => <String, Object?>{
        'hash': hash,
        'state': state.toJson(),
      };
}

/// How an installed host build is doing.
enum BuildState {
  noRecord('NoRecord'),
  pending('Pending'),
  probation('Probation'),
  good('Good'),
  rolledBack('RolledBack'),
  failed('Failed');

  const BuildState(this.wire);

  /// The name serde writes.
  final String wire;

  static BuildState fromJson(Object? json) {
    for (final value in values) {
      if (value.wire == json) return value;
    }
    throw FormatException('Unknown BuildState: $json');
  }

  String toJson() => wire;
}

/// A device paired with this PC.
class PairedDevice {
  const PairedDevice({
    required this.id,
    required this.name,
    required this.connected,
    required this.allowed,
    required this.lastSeen,
    this.vouchedBy,
    required this.mayVouch,
  });

  factory PairedDevice.fromJson(Object? json) {
    final map = jsonMap(json);
    final vouchedBy = map['vouched_by'];
    return PairedDevice(
      id: jsonString(jsonField(map, 'id')),
      name: jsonString(jsonField(map, 'name')),
      connected: jsonBool(jsonField(map, 'connected')),
      allowed: jsonString(jsonField(map, 'allowed')),
      lastSeen: jsonString(jsonField(map, 'last_seen')),
      vouchedBy: jsonStringOrNull(vouchedBy),
      mayVouch: jsonBool(jsonField(map, 'may_vouch')),
    );
  }

  final String id;
  final String name;
  final bool connected;

  /// Display text, e.g. "View, input".
  final String allowed;

  /// Display text, e.g. "Seen today", "Not seen for 41 days".
  final String lastSeen;
  final String? vouchedBy;
  final bool mayVouch;

  Object? toJson() => <String, Object?>{
        'id': id,
        'name': name,
        'connected': connected,
        'allowed': allowed,
        'last_seen': lastSeen,
        'vouched_by': vouchedBy,
        'may_vouch': mayVouch,
      };
}

/// An incoming pairing request on this PC.
class PairingRequest {
  const PairingRequest({
    required this.requestId,
    required this.deviceName,
    required this.deviceId,
    required this.number,
    required this.expiresInSecs,
  });

  factory PairingRequest.fromJson(Object? json) {
    final map = jsonMap(json);
    return PairingRequest(
      requestId: jsonString(jsonField(map, 'request_id')),
      deviceName: jsonString(jsonField(map, 'device_name')),
      deviceId: jsonString(jsonField(map, 'device_id')),
      number: jsonInt(jsonField(map, 'number')),
      expiresInSecs: jsonInt(jsonField(map, 'expires_in_secs')),
    );
  }

  final String requestId;
  final String deviceName;
  final String deviceId;
  final int number;
  final int expiresInSecs;

  Object? toJson() => <String, Object?>{
        'request_id': requestId,
        'device_name': deviceName,
        'device_id': deviceId,
        'number': number,
        'expires_in_secs': expiresInSecs,
      };
}

/// A pairing request that was already answered (tray or another device).
class AnsweredRequest {
  const AnsweredRequest({
    required this.request,
    required this.answeredOn,
    required this.allowed,
  });

  factory AnsweredRequest.fromJson(Object? json) {
    final map = jsonMap(json);
    return AnsweredRequest(
      request: PairingRequest.fromJson(jsonField(map, 'request')),
      answeredOn: jsonString(jsonField(map, 'answered_on')),
      allowed: jsonBool(jsonField(map, 'allowed')),
    );
  }

  final PairingRequest request;
  final String answeredOn;
  final bool allowed;

  Object? toJson() => <String, Object?>{
        'request': request.toJson(),
        'answered_on': answeredOn,
        'allowed': allowed,
      };
}
