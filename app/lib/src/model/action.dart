import 'common.dart';
import 'host.dart';
import 'json_util.dart';
import 'session.dart';

/// What the UI sends back to the backend.
///
/// Serde is externally tagged: a unit variant is a bare string
/// (`"CancelFlow"`), every other variant a single-key object
/// (`{"SetMode":"Game"}`, `{"Connect":{"pc_id":"..."}}`).
sealed class Action {
  const Action();

  factory Action.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'Connect':
        {
          final map = jsonMap(variant.value);
          return Connect(pcId: jsonString(jsonField(map, 'pc_id')));
        }
      case 'CancelFlow':
        return const CancelFlow();
      case 'Retry':
        return const Retry();
      case 'RetryWebSocket':
        return const RetryWebSocket();
      case 'Forget':
        {
          final map = jsonMap(variant.value);
          return Forget(pcId: jsonString(jsonField(map, 'pc_id')));
        }
      case 'Rename':
        {
          final map = jsonMap(variant.value);
          return Rename(
            pcId: jsonString(jsonField(map, 'pc_id')),
            name: jsonString(jsonField(map, 'name')),
          );
        }
      case 'StartPairing':
        return const StartPairing();
      case 'StartRemotePairing':
        return const StartRemotePairing();
      case 'SubmitPairing':
        {
          final map = jsonMap(variant.value);
          return SubmitPairing(
            link: jsonStringOrNull(map['link']),
            pcId: jsonString(jsonField(map, 'pc_id')),
            code: jsonString(jsonField(map, 'code')),
            relay: jsonString(jsonField(map, 'relay')),
          );
        }
      case 'RequestRemotePairing':
        {
          final map = jsonMap(variant.value);
          return RequestRemotePairing(
            pcId: jsonString(jsonField(map, 'pc_id')),
            relay: jsonString(jsonField(map, 'relay')),
          );
        }
      case 'Vouch':
        {
          final map = jsonMap(variant.value);
          return Vouch(
            requestId: jsonString(jsonField(map, 'request_id')),
            accept: jsonBool(jsonField(map, 'accept')),
            permissions: jsonListOf(
              jsonField(map, 'permissions'),
              jsonString,
            ),
          );
        }
      case 'AnswerPairing':
        {
          final map = jsonMap(variant.value);
          return AnswerPairing(
            requestId: jsonString(jsonField(map, 'request_id')),
            allow: jsonBool(jsonField(map, 'allow')),
          );
        }
      case 'Revoke':
        {
          final map = jsonMap(variant.value);
          return Revoke(deviceId: jsonString(jsonField(map, 'device_id')));
        }
      case 'SetMayVouch':
        {
          final map = jsonMap(variant.value);
          return SetMayVouch(
            deviceId: jsonString(jsonField(map, 'device_id')),
            mayVouch: jsonBool(jsonField(map, 'may_vouch')),
          );
        }
      case 'SetHostSetting':
        return SetHostSetting(HostSetting.fromJson(variant.value));
      case 'StartHosting':
        return const StartHosting();
      case 'StopHosting':
        return const StopHosting();
      case 'RestartHost':
        return const RestartHost();
      case 'RollBackHost':
        return const RollBackHost();
      case 'OpenLogsFolder':
        return const OpenLogsFolder();
      case 'ShowPairDevice':
        return const ShowPairDevice();
      case 'CreateKey':
        return const CreateKey();
      case 'UnlockImport':
        {
          final map = jsonMap(variant.value);
          return UnlockImport(passphrase: jsonString(jsonField(map, 'passphrase')));
        }
      case 'ExportKey':
        return const ExportKey();
      case 'ImportKey':
        return const ImportKey();
      case 'ImportKeyText':
        {
          final map = jsonMap(variant.value);
          return ImportKeyText(
            fileName: jsonString(jsonField(map, 'file_name')),
            text: jsonString(jsonField(map, 'text')),
          );
        }
      case 'ReplaceKey':
        return const ReplaceKey();
      case 'SetMode':
        return SetMode(StreamMode.fromJson(variant.value));
      case 'SetMonitor':
        return SetMonitor(jsonInt(variant.value));
      case 'SetQuality':
        return SetQuality(Quality.fromJson(variant.value));
      case 'SetAudio':
        return SetAudio(jsonBool(variant.value));
      case 'Disconnect':
        return const Disconnect();
    }
    throw FormatException('Unknown Action variant: ${variant.name}');
  }

  Object? toJson();
}

// Your PCs and connecting

/// Connect to a saved PC.
class Connect extends Action {
  const Connect({required this.pcId});

  final String pcId;

  @override
  Object? toJson() => <String, Object?>{
        'Connect': <String, Object?>{'pc_id': pcId},
      };
}

/// Back out of the current flow.
class CancelFlow extends Action {
  const CancelFlow();

  @override
  Object? toJson() => 'CancelFlow';
}

/// Try the last connect again.
class Retry extends Action {
  const Retry();

  @override
  Object? toJson() => 'Retry';
}

/// Ask for a fresh WebSocket, keeping the session.
class RetryWebSocket extends Action {
  const RetryWebSocket();

  @override
  Object? toJson() => 'RetryWebSocket';
}

/// Forget a saved PC.
class Forget extends Action {
  const Forget({required this.pcId});

  final String pcId;

  @override
  Object? toJson() => <String, Object?>{
        'Forget': <String, Object?>{'pc_id': pcId},
      };
}

/// Rename a saved PC.
class Rename extends Action {
  const Rename({required this.pcId, required this.name});

  final String pcId;
  final String name;

  @override
  Object? toJson() => <String, Object?>{
        'Rename': <String, Object?>{'pc_id': pcId, 'name': name},
      };
}

// Pairing (this device joins a PC)

/// Show the pairing input.
class StartPairing extends Action {
  const StartPairing();

  @override
  Object? toJson() => 'StartPairing';
}

/// Switch the pairing input to a remote request (no code).
class StartRemotePairing extends Action {
  const StartRemotePairing();

  @override
  Object? toJson() => 'StartRemotePairing';
}

/// Submit a pairing link, or a PC ID, one-time code and relay.
class SubmitPairing extends Action {
  const SubmitPairing({
    this.link,
    required this.pcId,
    required this.code,
    required this.relay,
  });

  final String? link;
  final String pcId;
  final String code;
  final String relay;

  @override
  Object? toJson() => <String, Object?>{
        'SubmitPairing': <String, Object?>{
          'link': link,
          'pc_id': pcId,
          'code': code,
          'relay': relay,
        },
      };
}

/// Ask a PC with remote pairing on to pair with this device.
class RequestRemotePairing extends Action {
  const RequestRemotePairing({required this.pcId, required this.relay});

  final String pcId;
  final String relay;

  @override
  Object? toJson() => <String, Object?>{
        'RequestRemotePairing': <String, Object?>{
          'pc_id': pcId,
          'relay': relay,
        },
      };
}

// Vouching for another device on a remote PC

/// Answer a pairing request on a PC this device may vouch for.
class Vouch extends Action {
  const Vouch({
    required this.requestId,
    required this.accept,
    required this.permissions,
  });

  final String requestId;
  final bool accept;

  /// Permissions this device vouches for, e.g. ["view", "input"].
  final List<String> permissions;

  @override
  Object? toJson() => <String, Object?>{
        'Vouch': <String, Object?>{
          'request_id': requestId,
          'accept': accept,
          'permissions': [for (final permission in permissions) permission],
        },
      };
}

// This PC

/// Allow or deny an incoming pairing request on this PC.
class AnswerPairing extends Action {
  const AnswerPairing({required this.requestId, required this.allow});

  final String requestId;
  final bool allow;

  @override
  Object? toJson() => <String, Object?>{
        'AnswerPairing': <String, Object?>{
          'request_id': requestId,
          'allow': allow,
        },
      };
}

/// Revoke a device paired with this PC.
class Revoke extends Action {
  const Revoke({required this.deviceId});

  final String deviceId;

  @override
  Object? toJson() => <String, Object?>{
        'Revoke': <String, Object?>{'device_id': deviceId},
      };
}

/// Let a paired device (or not) vouch for others.
class SetMayVouch extends Action {
  const SetMayVouch({required this.deviceId, required this.mayVouch});

  final String deviceId;
  final bool mayVouch;

  @override
  Object? toJson() => <String, Object?>{
        'SetMayVouch': <String, Object?>{
          'device_id': deviceId,
          'may_vouch': mayVouch,
        },
      };
}

/// Change one host setting.
class SetHostSetting extends Action {
  const SetHostSetting(this.setting);

  final HostSetting setting;

  @override
  Object? toJson() => <String, Object?>{'SetHostSetting': setting.toJson()};
}

/// Start the host program.
class StartHosting extends Action {
  const StartHosting();

  @override
  Object? toJson() => 'StartHosting';
}

/// Stop the host program.
class StopHosting extends Action {
  const StopHosting();

  @override
  Object? toJson() => 'StopHosting';
}

/// Restart the host program.
class RestartHost extends Action {
  const RestartHost();

  @override
  Object? toJson() => 'RestartHost';
}

/// Roll the host back to the previous build.
class RollBackHost extends Action {
  const RollBackHost();

  @override
  Object? toJson() => 'RollBackHost';
}

/// Open the folder the host logs live in.
class OpenLogsFolder extends Action {
  const OpenLogsFolder();

  @override
  Object? toJson() => 'OpenLogsFolder';
}

/// Show the pairing window for a new device on this PC.
class ShowPairDevice extends Action {
  const ShowPairDevice();

  @override
  Object? toJson() => 'ShowPairDevice';
}

// This device's key

/// Create a device key.
class CreateKey extends Action {
  const CreateKey();

  @override
  Object? toJson() => 'CreateKey';
}

/// Answer [Flow] step `ImportUnlock`; the passphrase never leaves the device.
class UnlockImport extends Action {
  const UnlockImport({required this.passphrase});

  final String passphrase;

  @override
  Object? toJson() => <String, Object?>{
        'UnlockImport': <String, Object?>{'passphrase': passphrase},
      };
}

/// Save the device key to a file.
class ExportKey extends Action {
  const ExportKey();

  @override
  Object? toJson() => 'ExportKey';
}

/// Open the file picker and import a device key.
class ImportKey extends Action {
  const ImportKey();

  @override
  Object? toJson() => 'ImportKey';
}

/// Import a key file the host already read.
class ImportKeyText extends Action {
  const ImportKeyText({required this.fileName, required this.text});

  final String fileName;
  final String text;

  @override
  Object? toJson() => <String, Object?>{
        'ImportKeyText': <String, Object?>{'file_name': fileName, 'text': text},
      };
}

/// Replace the device key with a new one and tell the saved PCs.
class ReplaceKey extends Action {
  const ReplaceKey();

  @override
  Object? toJson() => 'ReplaceKey';
}

// Session

/// Switch between Work and Game.
class SetMode extends Action {
  const SetMode(this.mode);

  final StreamMode mode;

  @override
  Object? toJson() => <String, Object?>{'SetMode': mode.toJson()};
}

/// Stream another monitor, by index into [Session.monitors].
class SetMonitor extends Action {
  const SetMonitor(this.monitor);

  final int monitor;

  @override
  Object? toJson() => <String, Object?>{'SetMonitor': monitor};
}

/// Change the quality preset.
class SetQuality extends Action {
  const SetQuality(this.quality);

  final Quality quality;

  @override
  Object? toJson() => <String, Object?>{'SetQuality': quality.toJson()};
}

/// Turn the viewer's sound on or off.
class SetAudio extends Action {
  const SetAudio(this.on);

  final bool on;

  @override
  Object? toJson() => <String, Object?>{'SetAudio': on};
}

/// Leave the session.
class Disconnect extends Action {
  const Disconnect();

  @override
  Object? toJson() => 'Disconnect';
}
