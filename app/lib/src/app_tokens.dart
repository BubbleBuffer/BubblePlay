// BubblePlay's own layout values: the sizes particular to this app's screens. Shared steps
// (spacing, control heights, type, colours) come from bb_tokens; bb_lints accepts raw values
// only in token files like this one.

import 'package:flutter/widgets.dart';

/// The largest system text scale the app follows.
const maxTextScale = 2.0;

/// Widths and heights of BubblePlay's own surfaces.
abstract final class AppSizes {
  /// The centred column an enrolment step sits in on a wider window.
  static const double stepPage = 440;

  /// The selected PC's (or This PC's) detail pane beside the PC list. Wide enough that a
  /// paired device's row keeps its Revoke button beside the name (bb_ui's DeviceRow stacks
  /// it below `Widths.dialog` of room).
  static const double detailPane = 400;

  /// The share of the window's height a pairing or enrolment panel takes at most.
  static const double flowPaneHeight = .86;

  /// The settings panel on a wider window.
  static const double settingsWidth = 560, settingsHeight = 520;

  /// The value column of a settings row (the device ID) on a wider window.
  static const double settingsValue = 240;

  /// The feedback panel on a wider window.
  static const double feedbackWidth = 560;

  /// The feedback text field's smallest height (about three lines).
  static const double feedbackField = 96;

  /// The screenshot preview in the feedback form: in a bottom sheet, and in a panel.
  static const double screenshotSheet = 140, screenshotPanel = 120;

  /// What the feedback sheet keeps for its title, gaps and buttons below its scrolling body,
  /// and the smallest that body gets.
  static const double feedbackSheetChrome = 134, feedbackBodyMin = 120;

  /// The display picker panel's largest height on a wider window.
  static const double displayPicker = 420;

  /// Notices over the session video.
  static const double sessionNotice = 620;
}

/// Behind the session video: the picture's own black, the same in both themes.
const videoBackdrop = Color(0xFF000000);
