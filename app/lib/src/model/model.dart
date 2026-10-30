/// Hand-written models for the JSON contract shared with the Rust model in
/// `client-win/src/model.rs`.
///
/// Everything here is plain immutable Dart: no code generation. Each type has
/// [fromJson] and [toJson] and throws [FormatException] on an unknown variant.
library;

export 'action.dart';
export 'app_state.dart';
export 'common.dart';
export 'flow.dart';
export 'host.dart';
export 'json_util.dart';
export 'session.dart';
