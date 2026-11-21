import 'dart:async';

import 'package:bb_ui/bb_ui.dart';
import 'package:flutter/widgets.dart' hide Action, Flow;

import '../app.dart';
import '../app_tokens.dart';
import '../model/model.dart';
import '../devices/devices_controller.dart';
import 'pc_actions.dart';
import 'settings_screen.dart';
import 'this_pc_detail.dart';

/// Your PCs, with "This PC" pinned above them on Windows. An expanded window shows the list beside
/// the selected PC's details; a narrower one (a phone, a small window) shows the list alone and
/// opens a PC's actions from its row.
class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key, required this.state});

  final AppState state;

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  String? _selectedId;
  bool _thisPcSelected = false;
  StreamSubscription<void>? _shows;

  bool _started = false;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (_started) return;
    _started = true;
    final env = AppScope.of(context);
    final devices = env.devices;
    if (devices == null) return;
    // A `show` from the host (the tray's Devices item) or --section=this-pc picks This PC.
    _thisPcSelected = env.initialSection == 'this-pc' || devices.showPending;
    devices.consumeShow();
    _shows = devices.shows.listen((_) {
      devices.consumeShow();
      if (mounted) setState(() => _thisPcSelected = true);
    });
  }

  @override
  void dispose() {
    _shows?.cancel();
    super.dispose();
  }

  RemotePc? get _selected {
    final pcs = widget.state.pcs;
    for (final pc in pcs) {
      if (pc.id == _selectedId) return pc;
    }
    return pcs.isEmpty ? null : pcs.first;
  }

  @override
  Widget build(BuildContext context) {
    final env = AppScope.of(context);
    // The list beside the details needs an expanded window. A narrower one shows the list alone
    // and opens a PC's actions or This PC from its row.
    final compact = !context.sizeClass.atLeast(SizeClass.expanded);
    final list = _PcList(
      state: widget.state,
      showThisPc: env.isWindows,
      devices: env.devices,
      thisPcSelected: _thisPcSelected,
      onSelectThisPc: () {
        final devices = env.devices;
        if (compact && devices != null) {
          // Narrow window: the detail is a pushed page, like a PC's actions.
          pushScreen<void>(
            context,
            (_) => ScreenScaffold(title: 'This PC', body: ThisPcDetail(devices: devices)),
          );
          return;
        }
        setState(() => _thisPcSelected = true);
      },
      selectedId: _thisPcSelected ? null : _selected?.id,
      onSelect: (pc) => setState(() {
        _selectedId = pc.id;
        _thisPcSelected = false;
      }),
      compact: compact,
    );
    if (compact) return SafeArea(child: list);
    final selected = _selected;
    return Row(
      children: [
        Expanded(child: list),
        const VLine(height: double.infinity),
        SizedBox(
          width: AppSizes.detailPane,
          child: _thisPcSelected && env.devices != null
              ? ThisPcDetail(devices: env.devices!)
              : selected == null
              ? const SizedBox.shrink()
              : PcDetail(key: ValueKey(selected.id), pc: selected),
        ),
      ],
    );
  }
}

class _PcList extends StatelessWidget {
  const _PcList({
    required this.state,
    required this.showThisPc,
    required this.devices,
    required this.thisPcSelected,
    required this.onSelectThisPc,
    required this.selectedId,
    required this.onSelect,
    required this.compact,
  });

  final AppState state;
  final bool showThisPc;
  final DevicesController? devices;
  final bool thisPcSelected;
  final VoidCallback onSelectThisPc;
  final String? selectedId;
  final ValueChanged<RemotePc> onSelect;
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    final pcs = state.pcs;
    final pad = compact ? Layout.gutter : Space.xxl;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Expanded(
          child: ListView(
            padding: EdgeInsets.fromLTRB(pad, compact ? Space.lg : Space.xl, pad, Space.lg),
            children: [
              if (showThisPc) ...[
                const SectionLabel('This PC', icon: Ico.monitorPlay),
                const Gap(Space.sm),
                ThisPcRow(
                  host: state.host,
                  devices: devices,
                  selected: thisPcSelected,
                  onSelect: onSelectThisPc,
                ),
                const Gap(Space.lg),
                const HLine(),
                const Gap(Space.xl),
              ],
              SectionHeader(
                'Your PCs',
                subtitle: "Other PCs paired with this device's key.",
                trailing: compact
                    ? null
                    : IntrinsicWidth(
                        child: SecondaryButton(
                          label: 'Pair a new PC',
                          icon: Ico.plus,
                          size: ControlSize.lg,
                          onTap: () => controller.act(const StartPairing()),
                        ),
                      ),
              ),
              const Gap(Space.md),
              if (pcs.isEmpty)
                EmptyState(
                  icon: Ico.monitor,
                  title: 'No PCs yet',
                  message: 'Pair a PC to stream from it.',
                  action: PrimaryButton(
                    label: 'Pair a PC',
                    onTap: () => controller.act(const StartPairing()),
                  ),
                )
              else
                for (final pc in pcs)
                  PcRow(
                    key: ValueKey(pc.id),
                    pc: pc,
                    selected: pc.id == selectedId,
                    compact: compact,
                    onSelect: () => onSelect(pc),
                  ),
            ],
          ),
        ),
        const HLine(),
        _DeviceFooter(state: state, compact: compact),
      ],
    );
  }
}

/// The pinned "This PC" row. Hosting itself is not in this version, so without a host it says
/// so; with one it shows the host's name and run state.
class ThisPcRow extends StatelessWidget {
  const ThisPcRow({
    super.key,
    required this.host,
    this.devices,
    this.selected = false,
    this.onSelect,
  });

  final HostStatus? host;

  /// The host's Devices pipe; when set the row reflects it and opens the Devices detail.
  final DevicesController? devices;
  final bool selected;
  final VoidCallback? onSelect;

  @override
  Widget build(BuildContext context) {
    final devices = this.devices;
    if (devices != null) {
      return ListenableBuilder(
        listenable: devices,
        builder: (context, _) {
          final state = devices.state;
          final (label, kind) = switch (state.connection) {
            LinkConnecting() => ('Connecting', StatusKind.attention),
            LinkDisconnected() => ('Not connected', StatusKind.offline),
            LinkConnected() => state.relayOnline
                ? ('Relay online', StatusKind.online)
                : ('Relay offline', StatusKind.attention),
          };
          final count = state.devices.length;
          return DeviceRow(
            icon: Ico.server,
            title: state.pcName ?? 'This PC',
            status: StatusPill(label, kind: kind),
            meta: [if (state.isConnected) count == 1 ? '1 paired device' : '$count paired devices'],
            selected: selected,
            onTap: onSelect,
          );
        },
      );
    }
    final host = this.host;
    if (host == null) {
      return const DeviceRow(
        icon: Ico.server,
        title: 'Hosting',
        status: StatusPill('Not connected'),
      );
    }
    final (label, kind) = switch (host.run) {
      HostStarting() => ('Starting', StatusKind.attention),
      HostRunning() => ('Hosting', StatusKind.online),
      HostRestarting() => ('Restarting', StatusKind.attention),
      HostStopped() => ('Stopped', StatusKind.offline),
      HostNotConfigured() => ('Not set up', StatusKind.neutral),
    };
    final viewer = host.viewer;
    return DeviceRow(
      icon: Ico.server,
      title: host.pcName,
      status: StatusPill(label, kind: kind),
      meta: [if (viewer != null) '${viewer.name} connected'],
    );
  }
}

/// One of Your PCs.
class PcRow extends StatelessWidget {
  const PcRow({
    super.key,
    required this.pc,
    required this.selected,
    required this.compact,
    required this.onSelect,
  });

  final RemotePc pc;
  final bool selected;
  final bool compact;
  final VoidCallback onSelect;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    void connect() => controller.act(Connect(pcId: pc.id));
    return DeviceRow(
      icon: pc.kind == PcKind.laptop ? Ico.laptop : Ico.monitor,
      title: pc.name,
      status: pcStatusPill(pc),
      id: pc.id,
      meta: [pcLastText(pc)],
      selected: selected && !compact,
      onTap: compact ? connect : onSelect,
      trailing: compact
          ? IconBtn(
              Ico.more,
              size: ControlSize.xl,
              semanticLabel: 'Actions for ${pc.name}',
              onTap: () => showPcActions(context, pc),
            )
          : (selected
              ? PrimaryButton(
                  label: 'Connect',
                  size: ControlSize.md,
                  icon: Ico.monitorPlay,
                  onTap: connect,
                )
              : SecondaryButton(label: 'Connect', size: ControlSize.md, onTap: connect)),
    );
  }
}

class _DeviceFooter extends StatelessWidget {
  const _DeviceFooter({required this.state, required this.compact});

  final AppState state;
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context).controller;
    final id = state.thisDevice.id;
    final thisDevice = Row(
      children: [
        Text('This device', style: ts(context.type.uiSmall, context.colors.textSecondary)),
        const Gap(Space.sm),
        Flexible(child: CopyableText(id)),
      ],
    );
    final pad = compact ? Layout.gutter : Space.xxl;
    if (!compact) {
      return Padding(
        padding: EdgeInsets.symmetric(horizontal: pad, vertical: Space.md),
        child: Row(
          children: [
            Expanded(child: thisDevice),
            TextBtn('Settings', icon: Ico.settings, onTap: () => openSettings(context, state)),
          ],
        ),
      );
    }
    return Padding(
      padding: EdgeInsets.fromLTRB(pad, Space.md, pad, Space.lg),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          thisDevice,
          const Gap(Space.md),
          SecondaryButton(label: 'Settings', onTap: () => openSettings(context, state)),
          const Gap(Space.sm),
          PrimaryButton(
            label: 'Pair a PC',
            expand: true,
            onTap: () => controller.act(const StartPairing()),
          ),
        ],
      ),
    );
  }
}
