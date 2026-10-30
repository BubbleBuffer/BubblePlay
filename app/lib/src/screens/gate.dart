import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../model/model.dart';
import 'enroll_screen.dart';
import 'flow_layer.dart';
import 'home_screen.dart';
import 'session_screen.dart';

/// Picks the screen for the current snapshot: a live session, the first-run key setup, or the PC
/// list with the pairing flow over it.
class Gate extends StatelessWidget {
  const Gate({super.key});

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    return ListenableBuilder(
      listenable: controller,
      builder: (context, _) {
        final state = controller.state;
        if (state == null) {
          return Center(
            child: controller.error == null
                ? const Spinner(size: 24)
                : _CoreError(onRetry: controller.refresh),
          );
        }
        final session = state.session;
        if (session != null) return SessionScreen(state: state, session: session);
        final flow = state.flow;
        if (flow is Enroll) return EnrollScreen(state: state, step: flow.step);
        return Stack(
          children: [
            Positioned.fill(child: HomeScreen(state: state)),
            if (flow != null) Positioned.fill(child: FlowLayer(state: state, flow: flow)),
          ],
        );
      },
    );
  }
}

class _CoreError extends StatelessWidget {
  const _CoreError({required this.onRetry});

  final Future<void> Function() onRetry;

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.all(24),
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 420),
          child: NoticeBanner(
            kind: NoticeKind.danger,
            title: 'BubblePlay could not start',
            message: 'The app core did not answer. Restart the app.',
            actions: [TextBtn('Try again', onTap: onRetry)],
          ),
        ),
      );
}
