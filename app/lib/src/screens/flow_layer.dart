import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../model/model.dart';
import 'pairing/pair_confirm_view.dart';
import 'pairing/pair_enter_form.dart';

/// The pairing flow (enter code, compare number) over the PC list, and the fallback for flows
/// that need a streaming session.
class FlowLayer extends StatelessWidget {
  const FlowLayer({super.key, required this.state, required this.flow});

  final AppState state;
  final Flow flow;

  /// The panel's title: what this step is, or why it cannot run.
  String get _title => switch (flow) {
        PairEnter(:final remote) => remote ? 'Pair without a code' : 'Pair a new PC',
        PairConfirm() => 'Compare the number',
        _ => 'Not available yet',
      };

  /// The step itself. The form is keyed by the kind of flow so a new snapshot does not throw
  /// away what the person typed.
  Widget _step() => switch (flow) {
        final PairEnter enter => PairEnterForm(
            key: ValueKey(enter.remote ? 'pair-enter-remote' : 'pair-enter-code'),
            remote: enter.remote,
            error: enter.error,
          ),
        final PairConfirm confirm => PairConfirmView(confirm: confirm),
        _ => const _UnavailableStep(),
      };

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    void cancel() => controller.act(const CancelFlow());
    final step = _step();
    return Focus(
      autofocus: true,
      onKeyEvent: (node, event) {
        if (event is KeyDownEvent && event.logicalKey == LogicalKeyboardKey.escape) {
          cancel();
          return KeyEventResult.handled;
        }
        return KeyEventResult.ignored;
      },
      child: Stack(
        children: [
          Positioned.fill(
            child: GestureDetector(
              behavior: HitTestBehavior.opaque,
              onTap: () {},
              child: ColoredBox(color: context.colors.overlay),
            ),
          ),
          if (context.mobile)
            Positioned.fill(child: _PhonePane(title: _title, onBack: cancel, child: step))
          else
            Positioned.fill(child: _DesktopPane(title: _title, child: step)),
        ],
      ),
    );
  }
}

/// Desktop: a centred 480 px panel on the scrim, scrolling when the content is taller than the
/// window.
class _DesktopPane extends StatelessWidget {
  const _DesktopPane({required this.title, required this.child});

  final String title;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    return Center(
      child: ConstrainedBox(
        constraints: BoxConstraints(maxHeight: MediaQuery.sizeOf(context).height * 0.86),
        child: Container(
          width: 480,
          decoration: BoxDecoration(
            color: c.bgElevated,
            borderRadius: Radii.panel,
            border: Border.all(color: c.border),
            boxShadow: c.shadowMenu,
          ),
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(24),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              mainAxisSize: MainAxisSize.min,
              children: [
                SectionHeader(title),
                const SizedBox(height: 16),
                child,
              ],
            ),
          ),
        ),
      ),
    );
  }
}

/// Phone: the flow takes the whole screen, with a 44 px bar carrying the back arrow and title.
class _PhonePane extends StatelessWidget {
  const _PhonePane({required this.title, required this.onBack, required this.child});

  final String title;
  final VoidCallback onBack;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    return ColoredBox(
      color: c.bgPrimary,
      child: SafeArea(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            SizedBox(
              height: Layout.topbarHeight,
              child: Row(
                children: [
                  const SizedBox(width: 4),
                  IconBtn(Ico.chevronLeft, semanticLabel: 'Back', onTap: onBack),
                  const SizedBox(width: 4),
                  Expanded(
                    child: Text(
                      title,
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: ts(context.type.h3, c.textPrimary),
                    ),
                  ),
                  const SizedBox(width: 16),
                ],
              ),
            ),
            Expanded(
              child: ListView(
                padding: const EdgeInsets.fromLTRB(16, 8, 16, 24),
                children: [
                  SectionHeader(title, large: false),
                  const SizedBox(height: 12),
                  child,
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// Approval, connecting, failed, refused and key rotation all need a live stream, which this
/// version does not have.
class _UnavailableStep extends StatelessWidget {
  const _UnavailableStep();

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      mainAxisSize: MainAxisSize.min,
      children: [
        const NoticeBanner(
          kind: NoticeKind.attention,
          message: 'This step needs streaming, which this version does not have.',
        ),
        const SizedBox(height: 20),
        SecondaryButton(label: 'Close', onTap: () => controller.act(const CancelFlow())),
      ],
    );
  }
}
