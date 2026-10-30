/// How a PC is streamed.
enum StreamMode {
  work('Work'),
  game('Game');

  const StreamMode(this.wire);

  /// The name serde writes.
  final String wire;

  static StreamMode fromJson(Object? json) {
    for (final value in values) {
      if (value.wire == json) return value;
    }
    throw FormatException('Unknown StreamMode: $json');
  }

  String toJson() => wire;
}

/// What kind of machine a saved PC is.
enum PcKind {
  desktop('Desktop'),
  laptop('Laptop');

  const PcKind(this.wire);

  /// The name serde writes.
  final String wire;

  static PcKind fromJson(Object? json) {
    for (final value in values) {
      if (value.wire == json) return value;
    }
    throw FormatException('Unknown PcKind: $json');
  }

  String toJson() => wire;
}
