import 'common.dart';
import 'json_util.dart';

/// The connect / pair / rotate flow, when one is in progress.
sealed class Flow {
  const Flow();

  factory Flow.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'Enroll':
        return Enroll(EnrollStep.fromJson(variant.value));
      case 'PairEnter':
        {
          final map = jsonMap(variant.value);
          return PairEnter(
            remote: jsonBool(jsonField(map, 'remote')),
            error: jsonStringOrNull(map['error']),
          );
        }
      case 'PairConfirm':
        {
          final map = jsonMap(variant.value);
          return PairConfirm(
            pcName: jsonString(jsonField(map, 'pc_name')),
            pcId: jsonString(jsonField(map, 'pc_id')),
            number: jsonInt(jsonField(map, 'number')),
            waitingSecs: jsonInt(jsonField(map, 'waiting_secs')),
            remote: jsonBool(jsonField(map, 'remote')),
            outcome: Outcome.fromJson(jsonField(map, 'outcome')),
          );
        }
      case 'Approval':
        {
          final map = jsonMap(variant.value);
          return Approval(
            pcId: jsonString(jsonField(map, 'pc_id')),
            waitingSecs: jsonInt(jsonField(map, 'waiting_secs')),
            outcome: Outcome.fromJson(jsonField(map, 'outcome')),
          );
        }
      case 'Connecting':
        {
          final map = jsonMap(variant.value);
          return Connecting(
            pcId: jsonString(jsonField(map, 'pc_id')),
            mode: StreamMode.fromJson(jsonField(map, 'mode')),
            steps: jsonListOf(jsonField(map, 'steps'), ConnStep.fromJson),
          );
        }
      case 'Failed':
        {
          final map = jsonMap(variant.value);
          return Failed(
            pcId: jsonString(jsonField(map, 'pc_id')),
            steps: jsonListOf(jsonField(map, 'steps'), ConnStep.fromJson),
            error: ConnError.fromJson(jsonField(map, 'error')),
          );
        }
      case 'Refused':
        {
          final map = jsonMap(variant.value);
          return Refused(pcId: jsonString(jsonField(map, 'pc_id')));
        }
      case 'Rotate':
        {
          final map = jsonMap(variant.value);
          return Rotate(
            oldId: jsonString(jsonField(map, 'old_id')),
            newId: jsonString(jsonField(map, 'new_id')),
            pcs: jsonListOf(jsonField(map, 'pcs'), RotatePc.fromJson),
          );
        }
    }
    throw FormatException('Unknown Flow variant: ${variant.name}');
  }

  Object? toJson();
}

/// First start without a device key: create or import one.
class Enroll extends Flow {
  const Enroll(this.step);

  final EnrollStep step;

  @override
  Object? toJson() => <String, Object?>{'Enroll': step.toJson()};
}

/// Pairing input.
class PairEnter extends Flow {
  const PairEnter({required this.remote, this.error});

  /// False takes the PC ID and the one-time code from the tray (or the
  /// pairing link); true asks a PC with remote pairing on, by PC ID only.
  final bool remote;
  final String? error;

  @override
  Object? toJson() => <String, Object?>{
        'PairEnter': <String, Object?>{'remote': remote, 'error': error},
      };
}

/// The pairing wait: the comparison number is shown here.
class PairConfirm extends Flow {
  const PairConfirm({
    required this.pcName,
    required this.pcId,
    required this.number,
    required this.waitingSecs,
    required this.remote,
    required this.outcome,
  });

  final String pcName;
  final String pcId;
  final int number;
  final int waitingSecs;
  final bool remote;
  final Outcome outcome;

  @override
  Object? toJson() => <String, Object?>{
        'PairConfirm': <String, Object?>{
          'pc_name': pcName,
          'pc_id': pcId,
          'number': number,
          'waiting_secs': waitingSecs,
          'remote': remote,
          'outcome': outcome.toJson(),
        },
      };
}

/// PC is at the Windows sign-in screen: a fresh ntfy approval is needed.
class Approval extends Flow {
  const Approval({
    required this.pcId,
    required this.waitingSecs,
    required this.outcome,
  });

  final String pcId;
  final int waitingSecs;
  final Outcome outcome;

  @override
  Object? toJson() => <String, Object?>{
        'Approval': <String, Object?>{
          'pc_id': pcId,
          'waiting_secs': waitingSecs,
          'outcome': outcome.toJson(),
        },
      };
}

/// Steps of a connect attempt in progress.
class Connecting extends Flow {
  const Connecting({
    required this.pcId,
    required this.mode,
    required this.steps,
  });

  final String pcId;
  final StreamMode mode;
  final List<ConnStep> steps;

  @override
  Object? toJson() => <String, Object?>{
        'Connecting': <String, Object?>{
          'pc_id': pcId,
          'mode': mode.toJson(),
          'steps': [for (final step in steps) step.toJson()],
        },
      };
}

/// A connect attempt that ended badly.
class Failed extends Flow {
  const Failed({required this.pcId, required this.steps, required this.error});

  final String pcId;
  final List<ConnStep> steps;
  final ConnError error;

  @override
  Object? toJson() => <String, Object?>{
        'Failed': <String, Object?>{
          'pc_id': pcId,
          'steps': [for (final step in steps) step.toJson()],
          'error': error.toJson(),
        },
      };
}

/// The PC did not accept this device's key (revoked or never paired).
class Refused extends Flow {
  const Refused({required this.pcId});

  final String pcId;

  @override
  Object? toJson() => <String, Object?>{
        'Refused': <String, Object?>{'pc_id': pcId},
      };
}

/// The device key was rotated and each saved PC was told.
class Rotate extends Flow {
  const Rotate({
    required this.oldId,
    required this.newId,
    required this.pcs,
  });

  final String oldId;
  final String newId;
  final List<RotatePc> pcs;

  @override
  Object? toJson() => <String, Object?>{
        'Rotate': <String, Object?>{
          'old_id': oldId,
          'new_id': newId,
          'pcs': [for (final pc in pcs) pc.toJson()],
        },
      };
}

/// One step of enrolling this device.
sealed class EnrollStep {
  const EnrollStep();

  factory EnrollStep.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'Choose':
        return const Choose();
      case 'Created':
        return const Created();
      case 'ImportUnlock':
        {
          final map = jsonMap(variant.value);
          return ImportUnlock(
            fileName: jsonString(jsonField(map, 'file_name')),
            error: jsonStringOrNull(map['error']),
          );
        }
      case 'ImportRefused':
        {
          final map = jsonMap(variant.value);
          return ImportRefused(
            fileName: jsonString(jsonField(map, 'file_name')),
            reason: jsonString(jsonField(map, 'reason')),
          );
        }
    }
    throw FormatException('Unknown EnrollStep variant: ${variant.name}');
  }

  Object? toJson();
}

/// No key: Create key or Import key file.
class Choose extends EnrollStep {
  const Choose();

  @override
  Object? toJson() => 'Choose';
}

/// A new key was made.
class Created extends EnrollStep {
  const Created();

  @override
  Object? toJson() => 'Created';
}

/// The chosen key file has a passphrase; it is asked once.
class ImportUnlock extends EnrollStep {
  const ImportUnlock({required this.fileName, this.error});

  final String fileName;
  final String? error;

  @override
  Object? toJson() => <String, Object?>{
        'ImportUnlock': <String, Object?>{'file_name': fileName, 'error': error},
      };
}

/// The file is not a usable key.
class ImportRefused extends EnrollStep {
  const ImportRefused({required this.fileName, required this.reason});

  final String fileName;
  final String reason;

  @override
  Object? toJson() => <String, Object?>{
        'ImportRefused': <String, Object?>{
          'file_name': fileName,
          'reason': reason,
        },
      };
}

/// Where an approval stands.
sealed class Outcome {
  const Outcome();

  factory Outcome.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'Waiting':
        return const Waiting();
      case 'Approved':
        return Approved(by: Answerer.fromJson(_by(jsonMap(variant.value))));
      case 'Denied':
        return Denied(by: Answerer.fromJson(_by(jsonMap(variant.value))));
      case 'TimedOut':
        return const TimedOut();
      case 'Cancelled':
        return const Cancelled();
    }
    throw FormatException('Unknown Outcome variant: ${variant.name}');
  }

  static Object? _by(Map<String, Object?> map) => jsonField(map, 'by');

  Object? toJson();
}

/// Nobody has answered yet.
class Waiting extends Outcome {
  const Waiting();

  @override
  Object? toJson() => 'Waiting';
}

/// Someone approved the request.
class Approved extends Outcome {
  const Approved({required this.by});

  final Answerer by;

  @override
  Object? toJson() => <String, Object?>{
        'Approved': <String, Object?>{'by': by.toJson()},
      };
}

/// Someone denied the request.
class Denied extends Outcome {
  const Denied({required this.by});

  final Answerer by;

  @override
  Object? toJson() => <String, Object?>{
        'Denied': <String, Object?>{'by': by.toJson()},
      };
}

/// Nobody answered before the request expired.
class TimedOut extends Outcome {
  const TimedOut();

  @override
  Object? toJson() => 'TimedOut';
}

/// The request was closed at the PC before anyone answered.
class Cancelled extends Outcome {
  const Cancelled();

  @override
  Object? toJson() => 'Cancelled';
}

/// Who answered a pairing request.
sealed class Answerer {
  const Answerer();

  factory Answerer.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'Pc':
        return const Pc();
      case 'Device':
        return Device(jsonString(variant.value));
    }
    throw FormatException('Unknown Answerer variant: ${variant.name}');
  }

  Object? toJson();
}

/// Someone at the PC (tray or Devices window).
class Pc extends Answerer {
  const Pc();

  @override
  Object? toJson() => 'Pc';
}

/// A paired device, by name (a voucher).
class Device extends Answerer {
  const Device(this.name);

  final String name;

  @override
  Object? toJson() => <String, Object?>{'Device': name};
}

/// Why a connect attempt ended, for [Failed].
sealed class ConnError {
  const ConnError();

  factory ConnError.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'NoFirstFrame':
        {
          final map = jsonMap(variant.value);
          return NoFirstFrame(secs: jsonInt(jsonField(map, 'secs')));
        }
      case 'RelayUnreachable':
        return const RelayUnreachable();
      case 'PcBusy':
        return const PcBusy();
      case 'Lost':
        {
          final map = jsonMap(variant.value);
          return Lost(attempts: jsonInt(jsonField(map, 'attempts')));
        }
      case 'NoHardwareDecoder':
        {
          final map = jsonMap(variant.value);
          return NoHardwareDecoder(
            codec: jsonString(jsonField(map, 'codec')),
            detail: jsonString(jsonField(map, 'detail')),
          );
        }
      case 'PcKeyChanged':
        return const PcKeyChanged();
    }
    throw FormatException('Unknown ConnError variant: ${variant.name}');
  }

  Object? toJson();
}

/// Key accepted, stream negotiated, but no video within [secs].
class NoFirstFrame extends ConnError {
  const NoFirstFrame({required this.secs});

  final int secs;

  @override
  Object? toJson() => <String, Object?>{
        'NoFirstFrame': <String, Object?>{'secs': secs},
      };
}

/// The relay did not answer.
class RelayUnreachable extends ConnError {
  const RelayUnreachable();

  @override
  Object? toJson() => 'RelayUnreachable';
}

/// Another viewer is connected (one viewer per PC).
class PcBusy extends ConnError {
  const PcBusy();

  @override
  Object? toJson() => 'PcBusy';
}

/// A running session dropped and reconnecting gave up.
class Lost extends ConnError {
  const Lost({required this.attempts});

  final int attempts;

  @override
  Object? toJson() => <String, Object?>{
        'Lost': <String, Object?>{'attempts': attempts},
      };
}

/// This computer has no hardware decoder for the stream's codec.
class NoHardwareDecoder extends ConnError {
  const NoHardwareDecoder({required this.codec, required this.detail});

  final String codec;

  /// The one-line reason from `media::NoHardwareVideoDecoder`.
  final String detail;

  @override
  Object? toJson() => <String, Object?>{
        'NoHardwareDecoder': <String, Object?>{'codec': codec, 'detail': detail},
      };
}

/// The PC's key does not match the pinned one; nothing was sent.
class PcKeyChanged extends ConnError {
  const PcKeyChanged();

  @override
  Object? toJson() => 'PcKeyChanged';
}

/// One line of the connect progress list.
class ConnStep {
  const ConnStep({
    required this.label,
    required this.detail,
    required this.status,
  });

  factory ConnStep.fromJson(Object? json) {
    final map = jsonMap(json);
    return ConnStep(
      label: jsonString(jsonField(map, 'label')),
      detail: jsonString(jsonField(map, 'detail')),
      status: StepStatus.fromJson(jsonField(map, 'status')),
    );
  }

  final String label;

  /// Evidence text on the right, e.g. relay host or "1.4 s".
  final String detail;
  final StepStatus status;

  Object? toJson() => <String, Object?>{
        'label': label,
        'detail': detail,
        'status': status.toJson(),
      };
}

/// Where a step of a connect attempt or a key rotation stands.
enum StepStatus {
  done('Done'),
  now('Now'),
  failed('Failed'),
  waiting('Waiting');

  const StepStatus(this.wire);

  /// The name serde writes.
  final String wire;

  static StepStatus fromJson(Object? json) {
    for (final value in values) {
      if (value.wire == json) return value;
    }
    throw FormatException('Unknown StepStatus: $json');
  }

  String toJson() => wire;
}

/// One PC in the key-rotation list.
class RotatePc {
  const RotatePc({
    required this.name,
    required this.status,
    required this.detail,
  });

  factory RotatePc.fromJson(Object? json) {
    final map = jsonMap(json);
    return RotatePc(
      name: jsonString(jsonField(map, 'name')),
      status: StepStatus.fromJson(jsonField(map, 'status')),
      detail: jsonString(jsonField(map, 'detail')),
    );
  }

  final String name;
  final StepStatus status;
  final String detail;

  Object? toJson() => <String, Object?>{
        'name': name,
        'status': status.toJson(),
        'detail': detail,
      };
}
