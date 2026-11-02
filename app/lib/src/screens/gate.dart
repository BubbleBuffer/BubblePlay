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
        final Widget screen;
        if (session != null) {
          screen = SessionScreen(state: state, session: session);
        } else if (state.flow is Enroll) {
          screen = EnrollScreen(state: state, step: (state.flow as Enroll).step);
        } else {
          final flow = state.flow;
          screen = Stack(
            children: [
              Positioned.fill(child: HomeScreen(state: state)),
              if (flow != null) Positioned.fill(child: FlowLayer(state: state, flow: flow)),
            ],
          );
        }
        if (controller.error == null) return screen;
        // An action the core could not run: say so instead of looking as if it worked.
        return Stack(
          children: [
            Positioned.fill(child: screen),
            Positioned(
              left: 16,
              right: 16,
              top: 16,
              child: SafeArea(
                child: Align(
                  alignment: Alignment.topCenter,
                  child: ConstrainedBox(
                    constraints: const BoxConstraints(maxWidth: 480),
                    child: NoticeBanner(
                      kind: NoticeKind.danger,
                      title: 'That did not work',
                      message: 'The app core could not run it. Try again.',
                      actions: [TextBtn('Dismiss', onTap: controller.dismissError)],
                    ),
                  ),
                ),
              ),
            ),
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
