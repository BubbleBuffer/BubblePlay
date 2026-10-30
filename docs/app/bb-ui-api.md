# bb_ui 0.7.0 API cheat sheet

Source: `C:\Users\Admin\AppData\Local\Pub\Cache\git\bb-ui-a7bd22519ca7f55c5b2bf9ddf039d5194dc3dc5d\packages\bb_ui\lib` (barrel `bb_ui.dart`).
Tokens: `...\packages\bb_tokens\lib\tokens.dart` (re-exported by bb_ui). Repo root has no CHANGELOG; docs are `README.md`, `packages/bb_ui/README.md`, `docs/verification.md`.
Signatures copied from source. Every widget class accepts `key` (omitted from the lines below).

## Root setup

- No Material anywhere: grep of `packages/` and `apps/` finds no `package:flutter/material`. Imports are `widgets`, `services`, `gestures`, `rendering`.
- Root is a `WidgetsApp`, not `MaterialApp`. Its `builder` wraps the Navigator in `AppTheme`, as `apps/gallery/lib/gallery.dart:32-50` does:

```dart
import 'package:bb_ui/bb_ui.dart';

WidgetsApp(
  title: 'App',
  color: AppColors.light.desk,
  debugShowCheckedModeBanner: false,
  pageRouteBuilder: <T>(settings, builder) => PageRouteBuilder<T>(
    settings: settings,
    pageBuilder: (context, animation, secondary) => builder(context),
  ),
  builder: (context, child) => LayoutBuilder(
    builder: (context, constraints) => AppTheme(
      colors: dark ? AppColors.darkTheme : AppColors.light, // switch here
      formFactor: FormFactor.forWidth(constraints.maxWidth), // breakpoint 768
      child: child!,
    ),
  ),
  home: HomeScreen(),
);
```

- `AppTheme({required AppColors colors, required FormFactor formFactor, required Widget child})` is an `InheritedWidget` (`bb_tokens/lib/tokens.dart:403`). Read it with `AppTheme.of(context)`. There is no `BbTheme`.
- Context accessors (`ThemeX`): `context.colors`, `context.type` (TypeScale), `context.formFactor`, `context.mobile`.
- Light/dark: neither package follows the system brightness (grep for `platformBrightness` finds nothing). The host passes `AppColors.light` or `AppColors.darkTheme` (`tokens.dart:195-197`) and rebuilds. `AppColors.dark` is a bool field.
- `FormFactor { desktop, mobile }`, `FormFactor.forWidth(double)`, `FormFactor.breakpoint = 768`.
- Fonts: the host registers the three families. Top-level mutable Strings: `sansFamily = 'Atkinson Hyperlegible Next'`, `monoFamily = 'Fragment Mono'`, `displayFamily = 'Bricolage Grotesque'`, plus `monoFallback = ['monospace']`. Assets and licences are in `apps/gallery/pubspec.yaml`: `AtkinsonHyperlegibleNext[wght].ttf` and `-Italic`, `FragmentMono-Regular.ttf` and `-Italic`, `BricolageGrotesque[opsz,wdth,wght].ttf`.
- Icons: `lucide_icons_flutter 3.1.25` is a bb_ui dependency. The app does not register it by hand. The golden config loads it for tests only (inference: at runtime Flutter bundles dependency fonts itself).
- `scrollBehavior`: `AppScrollBehavior` (routes.dart:46) is exported but used nowhere in the repo. Pass `scrollBehavior: const AppScrollBehavior()` to `WidgetsApp` yourself if you want its overscroll and scrollbar behaviour.
- Fully transparent colour: use `kTransparent` (`Colors.transparent` is Material).

## Tokens (bb_tokens, via bb_ui `tokens.dart`)

- `AppColors` (`tokens.dart:95`), built only through `AppColors.light` / `AppColors.darkTheme`. Roles: `palette` (ProfileColors), `dark`; `bgPrimary bgSecondary bgTertiary bgElevated bgHover bgPressed bgSelectedBlock textSelection`; `textPrimary textSecondary textTertiary textDisabled`; `iconPrimary iconSecondary iconTertiary`; `border borderSubtle borderStrong borderTranslucent divider`; `accent accentHover accentText onAccent focus`; `inlineCodeBg inlineCodeText overlay desk`; `thread threadStrong`; `okBg infoBg attnBg dangerBg pawBg`; `rail rail2 railInk railMuted railLine`; shadows `seam seamHover stack` (List<BoxShadow>), with getters `shadowMenu`, `shadowSmall`.
- Raw colour literals belong only in the token source (`scripts/check-raw-colours.sh`). `ProfileColors` lives in the generated `src/colors.g.dart`; do not hand-edit it.
- Spacing: `KitSpacing.standard` (`KitSpacing({hair=2, xs=4, sm=8, md=12, lg=16, xl=24})`). Also `KitLayout.standard` (sizeMedium 600, sizeExpanded 1000, fitComposer 460, contextSplit 680, paneMinWidth 200, paneMinHeight 120, edgeTab 28, controlHeight 32, rowHeight 28).
- `Layout` constants: `contentWidth 720`, `textWidth 708`, `topbarHeight 44`, `sidebarWidth 240`, `sidebarRowHeight 30`, `gutter 18`, `minTapTarget 44`, `listRowHeight 44`, `indent 24`.
- `Radii`: `sm 4, md 6, lg 10, sheet 12` (doubles); `ctl`, `field`, `panel` (BorderRadius); `pill` (999).
- `Motion`: `fast 150ms, standard 220ms, sheet 250ms, wiggle 200ms`; curves `easing`, `ease`, `settle`. `Motion.of(context)` returns `MotionDurations`, which is zero when reduce-motion is on.
- Type: `TypeScale.desktop` / `TypeScale.mobile` with `pageTitle, h1, h2, h3, body, ui, uiSmall, code`, each a `TypeSpec(size, line, weight)`. Helpers: `ts(TypeSpec s, Color color, {int? weight, bool mono = false})`, `tsRaw(double size, double line, int weight, Color color)`, `tsDisplay(TypeSpec s, Color color, {int? weight})`, `fw(int)`.

## buttons.dart

- `IconBtn(Ico icon, {VoidCallback? onTap, double size = 28, double glyph = 20, Color? color, double radius = Radii.md, bool selected = false, double? width, String? semanticLabel, String? tooltip, String? command})`: square icon button, optional tooltip or command shortcut.
- `PrimaryButton({required String label, required VoidCallback? onTap, double height = 42, Ico? icon, bool expand = false})`: accent button; a null onTap draws it disabled.
- `SecondaryButton({required String label, VoidCallback? onTap, double height = 42, Ico? icon, bool danger = false})`: thread-bordered secondary action.
- `StacksCheckbox({required bool checked, double size = 22, ValueChanged<bool>? onChanged, Color? fill, Color? ink})`: the shared task box.
- `TextBtn(String label, {VoidCallback? onTap, double height = 28, Color? color, Ico? icon})`: text-only button on a hover surface.

## fields.dart and CodeField

- `PlainField({required TextEditingController controller, required FocusNode focusNode, required TextStyle style, required String placeholder, ValueChanged<String>? onChanged, ValueChanged<String>? onSubmitted, bool autofocus = false, TextInputAction textInputAction = TextInputAction.search, List<TextInputFormatter>? inputFormatters, TextCapitalization textCapitalization = TextCapitalization.none, bool autocorrect = true, TextInputType? keyboardType})`: single-line EditableText field.
- `SearchField({required TextEditingController controller, required FocusNode focusNode, required ValueChanged<String> onChanged, String? placeholder, Key? fieldKey})`: search pill with clear button.
- `FieldFrame({required Widget child, double? height, EdgeInsetsGeometry padding = const EdgeInsets.symmetric(horizontal: 8)})`: border and focus ring around any field.
- `CodeField({required TextEditingController controller, required List<int> groups, String separator = '-', String placeholder = '', String? semanticLabel, ValueChanged<String>? onChanged, ValueChanged<String>? onSubmitted, bool autofocus = false, Key? fieldKey})`: monospace upper-case code entry; separators are inserted as the person types or pastes (e.g. `groups: [4, 4, 4, 4]`).
- `CodeFormatter(List<int> groups, {String separator = '-'})` extends `TextInputFormatter`; `String codeCharacters(String text, List<int> groups)`; `String formatCode(String text, List<int> groups, {String separator = '-'})`: the same rules without the widget (pairing/code_field.dart).
- `SettingsField` is in settings_controls (below).

## feedback.dart

- `enum NoticeKind { info, success, attention, danger }`
- `NoticeBanner({required String message, String? title, NoticeKind kind = NoticeKind.info, List<Widget> actions = const [], VoidCallback? onDismiss})`: inline message.
- `EmptyState({required Ico icon, required String title, String? message, Widget? action})`: empty list or screen.
- `ToastCard({required String title, String? message, Widget? content, Ico? icon, double? progress, double width = 420})`: the card alone.
- `ToastHandle` (public class; `dismiss()`): returned by
- `ToastHandle showToast(BuildContext context, {required Widget Function(BuildContext context, double? progress) builder, Duration duration = const Duration(seconds: 4), Alignment alignment = Alignment.topCenter, EdgeInsets margin = const EdgeInsets.only(top: 56), bool showProgress = true})`: root overlay, removes itself.

## modal.dart

- `Future<T?> showModalPanel<T>(BuildContext context, WidgetBuilder builder, {double width = 720, double height = 640, bool shrink = false})`: centred panel; Escape or a barrier tap closes it.
- `Future<bool> showConfirm(BuildContext context, {required String title, String? message, required String confirmLabel, String cancelLabel = 'Cancel', bool danger = false})`: completes true when confirmed.
- `ConfirmBox({required String title, String? message, required String confirmLabel, required ValueChanged<bool> onDone, String cancelLabel = 'Cancel', bool danger = false})`: the dialog body used by showConfirm.
- `Future<T?> pushScreen<T>(BuildContext context, WidgetBuilder builder, {String? name})`: mobile full-screen page route.
- `ScreenScaffold({required String title, required Widget body, List<Widget> actions = const []})`: 44 px bar with back, title, actions.
- `DialogButton({required String label, required VoidCallback? onTap, Color? color, bool filled = false, double? height})`: full-width dialog button.
- `Spinner({double size = 16, Color? color})`: rotating arc for work in progress.
- `InputBox({required Widget child, double? height})`: bordered input box.
- `PanelRow({required Widget child, VoidCallback? onTap, bool selected = false, EdgeInsetsGeometry? padding})`: tappable panel list row.

## sheet.dart

- `Future<T?> showSheet<T>(BuildContext context, WidgetBuilder builder)`: bottom sheet.
- `SheetSurface({required List<Widget> children})`: rounded top, grab handle, drag down to close.
- `SheetHeader(String text)`: grey group label.
- `SheetItem({required String label, required VoidCallback onTap, Ico? icon, Widget? leading, bool selected = false, bool danger = false})`: 48 px row.

## picker_sheet.dart

- `Future<T?> showPickerSheet<T>(BuildContext context, WidgetBuilder builder, {double desktopWidth = 480, double desktopHeight = 640, bool shrink = true, bool desktopScroll = false, double? mobileHeightFactor})`: phone gets a bottom sheet, desktop a modal panel.
- `PickerSearch({required TextEditingController controller, required FocusNode focusNode, required ValueChanged<String> onChanged, String? placeholder, Key? fieldKey})`: plain class, not a widget.
- `PickerGroup({required String label, required List<Widget> children})`: grouped rows under a grey label.
- `PickerSheet({required Widget list, Widget? header, Widget? above, PickerSearch? search, double? listHeight = 360, EdgeInsetsGeometry listPadding = const EdgeInsets.fromLTRB(8, 0, 8, 12), Key? listKey, Widget? footer})`: the picker layout.

## menu.dart

- `Future<T?> showPopover<T>(BuildContext context, Rect anchor, WidgetBuilder builder)`: popover under `anchor`.
- `Rect globalRectOf(BuildContext context)`: anchor rect for showPopover (SelectButton uses it too).
- `MenuSurface({required Widget child, double? radius, EdgeInsetsGeometry? padding, double? width})`: elevated menu surface.
- `MenuHeader(String text)`: grey label at the top of a section.
- `MenuItem({required String label, required VoidCallback? onTap, Ico? icon, Widget? leading, Widget? trailing, bool selected = false, bool danger = false, VoidCallback? onHover, bool firstLine = false})`: one menu row.
- `MenuSeparator()`: hairline.
- `MenuList({required List<Widget> children, double width = 265, String? header})`: menu on a MenuSurface.

## lines.dart

- `HLine({Color? color, double indent = 0})`: horizontal rule.
- `VLine({double height = 20})`: vertical rule.

## marked_list.dart

- `MarkedRow({required String label, required Color colour, VoidCallback? onTap, bool dot = false, bool small = false, bool oneLine = false, TextStyle? labelStyle, Widget? trailing, Widget? detail, EdgeInsetsGeometry padding = const EdgeInsets.symmetric(vertical: 5)})`: mark, label, trailing, detail.
- `MarkedList({required List<MarkedRow> rows, bool divided = false})`: column of rows, optional hairlines.

## key_chord.dart and palette Keycap

- `KeyChord(List<String> keys)`: shortcut as keycaps joined by "+".
- `Keycap(String label, {Color? color, bool raised = false})` (palette/command_palette.dart:362): one key box.

## hover.dart

- `Hover({required Widget child, VoidCallback? onTap, VoidCallback? onLongPress, VoidCallback? onSecondaryTap, double radius = Radii.md, BorderRadius? borderRadius, bool selected = false, Color? selectedColor, EdgeInsetsGeometry padding = EdgeInsets.zero, MouseCursor cursor = SystemMouseCursors.click, Color? hoverColor, ValueChanged<bool>? onHoverChanged, double lift = 1, List<BoxShadow>? shadow, List<BoxShadow>? hoverShadow, Color? background, Color? hoverBackground, Color? pressedBackground, BoxBorder? border})`: hover and focus surface for any row or tile.

## icons.dart and lucide.dart

- `enum Ico` (65 values, icons.dart:9-74): plus grip chevronLeft chevronRight chevronDown search page compose home inbox more trash arrowUp arrowDown turnInto duplicate link indent outdent keyboardHide undo bullets numbers todo quote code divider text check comment star clock settings sidebar close sendHorizontal hourglass terminal folder chevronUp monitor monitorPlay laptop server copy pencil refresh info circleCheck circleAlert triangleAlert pointer pointerClick activity gauge volume logOut gamepad qrCode globe radio arrowRight lock userCheck keyboard.
- `Icon20(Ico icon, {double size = 20, required Color color, double stroke = 1.5})`: Lucide icon for an Ico.
- `LucideIcon(IconData data, {required double size, required Color color, List<Offset>? fill})`: raw Lucide glyph.
- `void paintIco(Canvas canvas, Ico icon, Rect rect, Color color, {double stroke = 1.5})` and `void paintLucide(Canvas canvas, IconData data, Rect rect, Color color, {List<Offset>? fill})`: for CustomPainters.
- Public but internal: `typedef LucideGlyph`, `IconData lucideWeight(LucideGlyph g, double size, double stroke)`, `const icoGlyphs`, `lucideStarFill`, `lucidePlayFill`.

## routes.dart

- `Route<T> pageRoute<T>(WidgetBuilder builder, {required bool mobile, required MotionDurations motion, RouteSettings? settings, bool desktopSheet = false})`: the route pushScreen uses.
- `AppScrollBehavior()` (const): no glow overscroll, thin desktop scrollbar.

## section_header.dart

- `SectionHeader(String title, {String? subtitle, Widget? trailing, bool large = true})`: display-face title.
- `SectionLabel(String text, {Ico? icon})`: small label above a group.

## settings_controls.dart

- `SettingsHeading(String text, {double top = 0, bool primary = false})`: section title with hairline.
- `SettingsRow({required String label, String? description, Widget? trailing, bool stack = false})`: label and description with a trailing control.
- `Toggle({required bool value, required ValueChanged<bool>? onChanged})`: 30x18 switch.
- `SelectButton<T>({required T value, required List<T> options, required String Function(T) labelOf, required ValueChanged<T>? onChanged, String? title})`: value plus chevron that opens a popover.
- `SettingsField({required TextEditingController controller, String placeholder = '', ValueChanged<String>? onSubmitted, ValueChanged<String>? onChanged, bool mono = false, Key? fieldKey, bool autofocus = false})`: bordered settings text field.
- `OutlineButton({required String label, required VoidCallback? onTap, bool danger = false, Widget? icon})`: 1 px border button.
- `CopyableText(String text, {Key? textKey, bool inline = false, double size = 13, Color? color})`: monospace text with copy.
- `CopyButton(String text, {double size = 24, double glyph = 16})`: copies `text`, then shows a check.

## selection.dart

- `NoteSelectionControls extends TextSelectionControls`, and the instance `noteSelectionControls`: selection handles and toolbar for EditableText without Material.

## reference_chip.dart

- `ReferenceChip({String? label, List<String> segments = const [], required Color colour, VoidCallback? onTap})`: small colour-dot label for a place in running text.

## stack_card.dart

- `FoldableStackCard({required Color edge, required String title, String? label, Key? faceKey, Widget? peek, Widget? body, Widget? Function(bool open)? footer, bool stacked = false, bool initiallyOpen = false, bool? open, ValueChanged<bool>? onOpenChanged, int titleMaxLines = 2})`: folding card; `open` makes it controlled.
- `FooterAction({required String label, required VoidCallback? onTap, bool main = false})`: quiet footer action.

## tooltip.dart

- `AppTooltip({required String label, String? shortcut, String? command, required Widget child})`: hover tooltip (500 ms delay, 1500 ms touch linger). The host sets `AppTooltip.commandShortcuts` (`List<SingleActivator>? Function(String id)`); the default returns null.
- `String shortcutLabel(SingleActivator a)` ("Ctrl+Shift+F"); `String? commandShortcutLabel(String id)`.

## devices/

- `enum StatusKind { online, offline, active, attention, danger, neutral }`; `StatusPill(String label, {StatusKind kind = StatusKind.neutral})`: status pill.
- `DeviceRow({required Ico icon, required String title, Widget? status, String? id, List<String> meta = const [], Widget? trailing, bool selected = false, VoidCallback? onTap})`: device list row.
- `KeyValueRow(String label, String value, {bool mono = false, double labelWidth = 88})`: label and value pair.
- `DetailPanel({required String title, String? eyebrow, String? id, List<Widget> rows = const [], List<Widget> actions = const []})`: facts about the selected item.

## pairing/

- `enum CodeDisplaySize { medium(22), large(44) }` (px).
- `CodeDisplay(String code, {required List<int> groups, String separator = '-', CodeDisplaySize size = CodeDisplaySize.medium, Color? color})`: large monospace code in groups.
- `OneTimeCode(String code, {required List<int> groups, String separator = '-', String? label, String? caption})`: code to type on another device, with copy.
- `ComparisonNumber(String digits, {int group = 3, String? label, String? caption})`: number two devices compare.
- `QrFrame({required Widget child, double size = 200, String? caption})`: light quiet-zone square; statics `QrFrame.paper`, `QrFrame.ink`.
- `CodeField`, `CodeFormatter`, `codeCharacters`, `formatCode`: see fields above.

## palette/

- `PaletteCommand({required String id, required String title, String subtitle = '', String? group, String? shortcutLabel, bool enabled = true, String? disabledReason, Ico? icon, Key? rowKey})`: plain class.
- `Future<PaletteCommand?> showCommandPalette(BuildContext context, {required List<PaletteCommand> commands, String initialQuery = '', String placeholder = 'Search commands…', String emptyNote = 'No matching commands'})`: opens the palette; completes with the command run, or null.
- `CommandPalette({required List<PaletteCommand> commands, required ValueChanged<PaletteCommand> onRun, required VoidCallback onClose, String initialQuery = '', String placeholder = 'Search commands…', String emptyNote = 'No matching commands', Key? fieldKey, bool autofocus = true})`: the panel itself, for hosts that place it.
- `FuzzyMatch? fuzzyMatch(String query, String text)`; `FuzzyMatch(int score, List<TextRange> ranges)` (fields `score`, `ranges`).

## search/

- `enum SearchTitleTone { normal, muted, standIn }`
- `SearchResult({required String id, required String title, SearchTitleTone titleTone = SearchTitleTone.normal, int titleMaxLines = 1, List<TextRange> titleHighlights = const [], String secondary = '', Key? secondaryKey, String snippet = '', Key? snippetKey, List<TextRange> snippetHighlights = const [], Ico? icon, Widget? leading, String? typeLabel, Key? rowKey})`: one row, a neutral view model.
- `SearchResultSection({required String id, required String label, required List<SearchResult> results})`
- `SearchTypeChip({required String id, required String label, required bool selected, required VoidCallback onTap, bool removable = false})`
- `List<TextRange> termHighlights(String text, List<String> terms)`: case-insensitive literal ranges.
- `HighlightedText(String text, {required TextStyle style, List<TextRange> highlights = const [], int maxLines = 1})`
- `SearchResultRow({required SearchResult result, required VoidCallback onTap, bool showTypeLabel = false})`; `static double extent(bool mobile)` (72 or 64).
- `SearchGroupHeader(SearchResultSection section)`
- `SearchResultsList({required List<SearchResultSection> sections, required ValueChanged<SearchResult> onPick, required bool mobile, int? active, EdgeInsets padding = EdgeInsets.zero})`
- `SearchTypeChips({required List<SearchTypeChip> chips})`
- `SearchPanel({required TextEditingController controller, required ValueChanged<String> onQueryChanged, required ValueChanged<SearchResult> onPick, required List<SearchResultSection>? sections, Key? fieldKey, String? placeholder, Widget? filters, String idleNote = '', String emptyNote = 'No results', String? narrowedNote, String? correction, String correctionPrefix = 'Showing results for ', Object? selectionResetKey})`: field, filters slot, grouped results; Up, Down and Enter.

## session/

- `EdgeTab({required String label, String? value, Color? mark, bool opened = false, VoidCallback? onTap})`: resting tab on the top edge.
- `EdgeStrip({required List<Widget> groups})`: open strip with seams between groups.
- `EdgeStripButton({required Ico icon, String? label, String? semanticLabel, bool selected = false, bool signal = false, bool menu = false, VoidCallback? onTap})`: strip control.
- `Metric(String label, String value)` (const); `Sparkline(List<double> values, {double height = 24, Color? color})`.
- `MetricsPanel({required String title, required List<Metric> metrics, String? hint, List<double> trend = const [], String? trendCaption, String? trendValue, double width = 270})`: evidence panel with sparkline.
- `DisplayInfo({required String id, required String label, required int width, required int height, int x = 0, int y = 0, String? detail})`: one screen in its own pixel coordinates.
- `DisplayPicker({required List<DisplayInfo> displays, required String selectedId, required ValueChanged<DisplayInfo> onSelect, String title = 'Show display', String? subtitle, String shownLabel = 'Showing', Widget? footer, double width = 340, bool autofocus = false})`: screens as a map and a list; keys 1 to 9 pick.

## Golden test helpers

- The package ships none. `packages/bb_ui/test/` (code_field, device_components, palette, search, tooltip) each wrap widgets in `AppTheme(...)` and `WidgetsApp(...)` by hand.
- `apps/gallery/test/flutter_test_config.dart`: `Future<void> testExecutable(FutureOr<void> Function() testMain)` binds the test binding, loads the three bundled families from `assets/fonts/`, and loads lucide_icons_flutter's fonts (private helpers `_loadPackageFonts`, `_packageRoot`).
- Golden tests `apps/gallery/test/*_golden_test.dart` are tagged `@Tags(['golden'])`. They pump `GalleryApp(initialBrightness: ..., scene: GalleryScene.x)` (`package:bb_ui_gallery/gallery.dart`; `GalleryScene { primitives, search, palette, devices, session }`) at widths 1000, 390 and 1600, then call `matchesGoldenFile('goldens/<name>.png')`. Run `flutter test --tags golden` in `apps/gallery`. Baselines come from the pinned Linux CI runner (`docs/verification.md`).
