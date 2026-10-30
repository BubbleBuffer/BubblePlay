import 'package:flutter/widgets.dart' hide Action, Flow;

import '../model/model.dart';

/// The pairing flow (enter code, compare number) over the PC list, and the fallback for flows
/// that need a streaming session.
class FlowLayer extends StatelessWidget {
  const FlowLayer({super.key, required this.state, required this.flow});

  final AppState state;
  final Flow flow;

  @override
  Widget build(BuildContext context) => const SizedBox.shrink();
}
