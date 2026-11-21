// The "Report a problem" sheet: a kind, a text box ("What happened?"), the screenshot of this
// screen (removable) and Send / Cancel. A bottom sheet on a phone, a panel on desktop. Escape
// closes it. It pops with true once [onSend] has stored the report; Cancel stores nothing.

import 'dart:math' as math;

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';

import '../app_tokens.dart';
import 'report.dart';

class FeedbackSheet extends StatefulWidget {
  const FeedbackSheet({
    super.key,
    required this.screenshot,
    required this.onSend,
    this.onStored,
  });

  /// The PNG of this screen, or null when there is none to attach.
  final Uint8List? screenshot;

  /// Stores the report; completes with true when it is stored.
  final Future<bool> Function(String text, FeedbackKind kind, Uint8List? screenshot) onSend;

  /// Called with the sheet's context once the report is stored, just before the sheet closes
  /// (the place to show the state line: this context is under the overlay).
  final void Function(BuildContext context)? onStored;

  @override
  State<FeedbackSheet> createState() => _FeedbackSheetState();
}

class _FeedbackSheetState extends State<FeedbackSheet> {
  final _text = TextEditingController();
  final _focus = FocusNode();
  late Uint8List? _screenshot = widget.screenshot;
  FeedbackKind _kind = FeedbackKind.bug;
  bool _sending = false;
  bool _failed = false;

  @override
  void dispose() {
    _text.dispose();
    _focus.dispose();
    super.dispose();
  }

  Future<void> _send() async {
    if (_sending) return;
    setState(() {
      _sending = true;
      _failed = false;
    });
    var stored = false;
    try {
      stored = await widget.onSend(_text.text, _kind, _screenshot);
    } catch (_) {}
    if (!mounted) return;
    if (stored) {
      widget.onStored?.call(context);
      Navigator.of(context).pop(true);
    } else {
      // The text stays as it was, for another try.
      setState(() {
        _sending = false;
        _failed = true;
      });
    }
  }

  void _cancel() => Navigator.of(context).maybePop();

  @override
  Widget build(BuildContext context) {
    final c = context.colors;
    final ty = context.type;
    // A bottom sheet in a narrow window (see FeedbackController); no keyboard pops up unasked
    // under a finger.
    final compact = context.sizeClass.isCompact;
    final touch = context.touch;
    final title = Semantics(
      header: true,
      child: Text('Report a problem', style: ts(ty.h3, c.textPrimary)),
    );
    final body = Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            Expanded(child: Text('Kind', style: ts(ty.ui, c.textSecondary))),
            SelectButton<FeedbackKind>(
              key: const ValueKey('feedback-kind'),
              value: _kind,
              options: FeedbackKind.values,
              labelOf: (kind) => kind.label,
              title: 'Kind',
              onChanged: _sending ? null : (kind) => setState(() => _kind = kind),
            ),
          ],
        ),
        const Gap(Space.md),
        ExcludeSemantics(
          child: Text('What happened?', style: ts(ty.ui, c.textSecondary)),
        ),
        const Gap(Space.xs),
        Semantics(
          label: 'What happened?',
          textField: true,
          multiline: true,
          child: ConstrainedBox(
            constraints: const BoxConstraints(minHeight: AppSizes.feedbackField),
            child: FieldFrame(
              padding: const EdgeInsets.all(Space.md),
              child: EditableText(
                key: const ValueKey('feedback-text'),
                controller: _text,
                focusNode: _focus,
                style: ts(ty.body, c.textPrimary),
                cursorColor: c.textPrimary,
                backgroundCursorColor: c.textTertiary,
                selectionColor: c.textSelection,
                cursorWidth: Stroke.regular,
                autofocus: !touch,
                keyboardType: TextInputType.multiline,
                textInputAction: TextInputAction.newline,
                selectionControls: noteSelectionControls,
                maxLines: null,
                minLines: 3,
              ),
            ),
          ),
        ),
        if (_screenshot case final shot?) ...[
          const Gap(Space.md),
          Align(
            alignment: Alignment.centerLeft,
            child: Container(
              padding: const EdgeInsets.all(Space.hair),
              decoration: BoxDecoration(
                border: Border.all(color: c.border),
                borderRadius: Radii.ctl,
              ),
              child: ClipRRect(
                borderRadius: Radii.ctl,
                child: ConstrainedBox(
                  constraints: BoxConstraints(
                    maxHeight: compact ? AppSizes.screenshotSheet : AppSizes.screenshotPanel,
                  ),
                  child: Image.memory(
                    shot,
                    key: const ValueKey('feedback-screenshot'),
                    fit: BoxFit.contain,
                    semanticLabel: 'Screenshot of this screen',
                    errorBuilder: (context, error, stack) => const SizedBox.shrink(),
                  ),
                ),
              ),
            ),
          ),
          Align(
            alignment: Alignment.centerLeft,
            child: TextBtn(
              'Remove screenshot',
              onTap: _sending ? null : () => setState(() => _screenshot = null),
            ),
          ),
        ],
        const Gap(Space.sm),
        Text(
          'Includes the recent log lines and errors from this device.',
          style: ts(ty.uiSmall, c.textTertiary, weight: Weight.regular),
        ),
        if (_failed) ...[
          const Gap(Space.md),
          const NoticeBanner(
            kind: NoticeKind.danger,
            message: 'The report could not be saved on this device.',
          ),
        ],
      ],
    );
    final buttons = Row(
      children: [
        Expanded(
          child: SecondaryButton(
            key: const ValueKey('feedback-cancel'),
            label: 'Cancel',
            onTap: _cancel,
          ),
        ),
        const Gap(Space.sm),
        Expanded(
          child: PrimaryButton(
            key: const ValueKey('feedback-send'),
            label: 'Send',
            onTap: _sending ? null : _send,
          ),
        ),
      ],
    );
    final Widget content;
    if (compact) {
      final media = MediaQuery.of(context);
      final room = media.size.height * Layout.sheetHeight -
          AppSizes.feedbackSheetChrome -
          media.padding.bottom;
      content = SheetSurface(
        children: [
          Padding(padding: const EdgeInsets.symmetric(horizontal: Layout.gutter), child: title),
          const Gap(Space.md),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: Layout.gutter),
            child: ConstrainedBox(
              constraints: BoxConstraints(
                maxHeight: math.max(room, AppSizes.feedbackBodyMin),
              ),
              child: SingleChildScrollView(child: body),
            ),
          ),
          const Gap(Space.lg),
          Padding(
            padding: const EdgeInsets.fromLTRB(Layout.gutter, 0, Layout.gutter, Space.sm),
            child: buttons,
          ),
        ],
      );
    } else {
      content = Padding(
        padding: const EdgeInsets.all(Space.xl),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            title,
            const Gap(Space.md),
            Flexible(child: SingleChildScrollView(child: body)),
            const Gap(Space.lg),
            buttons,
          ],
        ),
      );
    }
    return CallbackShortcuts(
      bindings: {const SingleActivator(LogicalKeyboardKey.escape): _cancel},
      child: content,
    );
  }
}
