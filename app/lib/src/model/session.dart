import 'common.dart';
import 'json_util.dart';

/// The live session with a remote PC, when connected (or reconnecting).
class Session {
  const Session({
    required this.pcId,
    required this.pcName,
    required this.mode,
    required this.rttMs,
    required this.monitors,
    required this.monitor,
    this.monitorPending = false,
    this.monitorError,
    required this.quality,
    this.qualityControlsAvailable = false,
    required this.audio,
    this.audioControlsAvailable = false,
    required this.captured,
    required this.link,
    this.notice,
    required this.stats,
  });

  factory Session.fromJson(Object? json) {
    final map = jsonMap(json);
    final notice = map['notice'];
    return Session(
      pcId: jsonString(jsonField(map, 'pc_id')),
      pcName: jsonString(jsonField(map, 'pc_name')),
      mode: StreamMode.fromJson(jsonField(map, 'mode')),
      rttMs: jsonInt(jsonField(map, 'rtt_ms')),
      monitors: jsonListOf(jsonField(map, 'monitors'), Monitor.fromJson),
      monitor: jsonInt(jsonField(map, 'monitor')),
      monitorPending: jsonBoolOr(map['monitor_pending'], false),
      monitorError: jsonStringOrNull(map['monitor_error']),
      quality: Quality.fromJson(jsonField(map, 'quality')),
      qualityControlsAvailable:
          jsonBoolOr(map['quality_controls_available'], false),
      audio: jsonBool(jsonField(map, 'audio')),
      audioControlsAvailable:
          jsonBoolOr(map['audio_controls_available'], false),
      captured: jsonBool(jsonField(map, 'captured')),
      link: Link.fromJson(jsonField(map, 'link')),
      notice: notice == null ? null : SessionNotice.fromJson(notice),
      stats: Stats.fromJson(jsonField(map, 'stats')),
    );
  }

  final String pcId;
  final String pcName;
  final StreamMode mode;
  final int rttMs;
  final List<Monitor> monitors;

  /// The index into [monitors] of the monitor being streamed.
  final int monitor;

  /// A monitor pick is waiting for the host to switch.
  final bool monitorPending;

  /// The last monitor pick the host refused or failed, as shown to the user.
  final String? monitorError;
  final Quality quality;

  /// Whether the quality picker can change anything here.
  final bool qualityControlsAvailable;
  final bool audio;

  /// Whether the audio toggle can change anything here.
  final bool audioControlsAvailable;

  /// Input capture is running.
  final bool captured;
  final Link link;
  final SessionNotice? notice;
  final Stats stats;

  Object? toJson() => <String, Object?>{
        'pc_id': pcId,
        'pc_name': pcName,
        'mode': mode.toJson(),
        'rtt_ms': rttMs,
        'monitors': [for (final monitor in monitors) monitor.toJson()],
        'monitor': monitor,
        'monitor_pending': monitorPending,
        'monitor_error': monitorError,
        'quality': quality.toJson(),
        'quality_controls_available': qualityControlsAvailable,
        'audio': audio,
        'audio_controls_available': audioControlsAvailable,
        'captured': captured,
        'link': link.toJson(),
        'notice': notice?.toJson(),
        'stats': stats.toJson(),
      };
}

/// One of the host's monitors, for the picker and the monitor map.
class Monitor {
  const Monitor({
    this.selector = '',
    required this.name,
    required this.width,
    required this.height,
    required this.refreshHz,
    required this.x,
    required this.y,
    required this.primary,
  });

  factory Monitor.fromJson(Object? json) {
    final map = jsonMap(json);
    return Monitor(
      selector: jsonStringOrNull(map['selector']) ?? '',
      name: jsonString(jsonField(map, 'name')),
      width: jsonInt(jsonField(map, 'width')),
      height: jsonInt(jsonField(map, 'height')),
      refreshHz: jsonInt(jsonField(map, 'refresh_hz')),
      x: jsonInt(jsonField(map, 'x')),
      y: jsonInt(jsonField(map, 'y')),
      primary: jsonBool(jsonField(map, 'primary')),
    );
  }

  /// The host's id for this monitor, sent back when it is picked.
  final String selector;
  final String name;
  final int width;
  final int height;
  final int refreshHz;

  /// Position in the host's desktop, for the monitor map.
  final int x;
  final int y;
  final bool primary;

  Object? toJson() => <String, Object?>{
        'selector': selector,
        'name': name,
        'width': width,
        'height': height,
        'refresh_hz': refreshHz,
        'x': x,
        'y': y,
        'primary': primary,
      };
}

/// The stream's quality preset.
enum Quality {
  auto('Auto'),
  sharp('Sharp'),
  smooth('Smooth');

  const Quality(this.wire);

  /// The name serde writes.
  final String wire;

  static Quality fromJson(Object? json) {
    for (final value in values) {
      if (value.wire == json) return value;
    }
    throw FormatException('Unknown Quality: $json');
  }

  String toJson() => wire;
}

/// The state of the connection to the host.
sealed class Link {
  const Link();

  factory Link.fromJson(Object? json) {
    final variant = JsonVariant.parse(json);
    switch (variant.name) {
      case 'Live':
        return const Live();
      case 'Reconnecting':
        {
          final map = jsonMap(variant.value);
          return Reconnecting(
            attempt: jsonInt(jsonField(map, 'attempt')),
            secs: jsonInt(jsonField(map, 'secs')),
          );
        }
    }
    throw FormatException('Unknown Link variant: ${variant.name}');
  }

  Object? toJson();
}

/// The stream is up.
class Live extends Link {
  const Live();

  @override
  Object? toJson() => 'Live';
}

/// The stream dropped and is being re-established.
class Reconnecting extends Link {
  const Reconnecting({required this.attempt, required this.secs});

  final int attempt;
  final int secs;

  @override
  Object? toJson() => <String, Object?>{
        'Reconnecting': <String, Object?>{'attempt': attempt, 'secs': secs},
      };
}

/// Something the PC wants the user to know about the session.
enum SessionNotice {
  /// Input to an elevated window is blocked until Admin mode is on.
  adminRequired('AdminRequired'),

  /// UAC prompt or sign-in screen on the PC.
  secureDesktop('SecureDesktop');

  const SessionNotice(this.wire);

  /// The name serde writes.
  final String wire;

  static SessionNotice fromJson(Object? json) {
    for (final value in values) {
      if (value.wire == json) return value;
    }
    throw FormatException('Unknown SessionNotice: $json');
  }

  String toJson() => wire;
}

/// The numbers under the session's stats panel.
class Stats {
  const Stats({
    required this.resolution,
    required this.fps,
    required this.bitrateMbps,
    required this.rttMs,
    required this.decodeMs,
    required this.dropped,
    required this.transport,
    required this.codec,
    required this.frameMs,
  });

  factory Stats.fromJson(Object? json) {
    final map = jsonMap(json);
    return Stats(
      resolution: jsonString(jsonField(map, 'resolution')),
      fps: jsonDouble(jsonField(map, 'fps')),
      bitrateMbps: jsonDouble(jsonField(map, 'bitrate_mbps')),
      rttMs: jsonInt(jsonField(map, 'rtt_ms')),
      decodeMs: jsonDouble(jsonField(map, 'decode_ms')),
      dropped: jsonInt(jsonField(map, 'dropped')),
      transport: jsonString(jsonField(map, 'transport')),
      codec: jsonString(jsonField(map, 'codec')),
      frameMs: jsonListOf(jsonField(map, 'frame_ms'), jsonDouble),
    );
  }

  final String resolution;
  final double fps;
  final double bitrateMbps;
  final int rttMs;
  final double decodeMs;
  final int dropped;
  final String transport;
  final String codec;

  /// Recent frame times in ms, oldest first, for the sparkline.
  final List<double> frameMs;

  Object? toJson() => <String, Object?>{
        'resolution': resolution,
        'fps': fps,
        'bitrate_mbps': bitrateMbps,
        'rtt_ms': rttMs,
        'decode_ms': decodeMs,
        'dropped': dropped,
        'transport': transport,
        'codec': codec,
        'frame_ms': [for (final ms in frameMs) ms],
      };
}
