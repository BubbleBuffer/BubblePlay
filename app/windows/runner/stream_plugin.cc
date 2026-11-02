#include "stream_plugin.h"

#include <flutter/plugin_registrar_windows.h>

#include <cstdint>
#include <optional>
#include <string>

namespace {

constexpr UINT_PTR kTickTimer = 0x42505354;  // "BPST"
constexpr UINT kTickMs = 8;

using ActiveFn = bool (*)();
using SetFrameCallbackFn = void (*)(void (*)());
using FrontFn = bool (*)(intptr_t*, uint32_t*, uint32_t*, uint64_t*);
using SetRectFn = void (*)(intptr_t, float, float, float, float);
using MessageFn = bool (*)(intptr_t, uint32_t, uintptr_t, intptr_t, intptr_t*);
using TickFn = void (*)(intptr_t);

double Number(const flutter::EncodableMap& map, const char* key) {
  auto it = map.find(flutter::EncodableValue(key));
  if (it == map.end()) return 0;
  if (const auto* d = std::get_if<double>(&it->second)) return *d;
  if (const auto* i = std::get_if<int32_t>(&it->second)) return *i;
  if (const auto* l = std::get_if<int64_t>(&it->second)) return static_cast<double>(*l);
  return 0;
}

}  // namespace

struct StreamPlugin::Core {
  HMODULE module = nullptr;
  ActiveFn active = nullptr;
  SetFrameCallbackFn set_frame_callback = nullptr;
  FrontFn front = nullptr;
  SetRectFn set_rect = nullptr;
  MessageFn message = nullptr;
  TickFn tick = nullptr;

  bool ok() const {
    return module && active && set_frame_callback && front && set_rect &&
           message && tick;
  }
};

std::atomic<int64_t> StreamPlugin::marked_id_{-1};
std::atomic<flutter::TextureRegistrar*> StreamPlugin::marked_registrar_{nullptr};

StreamPlugin::StreamPlugin(flutter::FlutterEngine* engine, HWND window)
    : window_(window) {
  core_ = std::make_unique<Core>();
  // The same module Dart loads through the bridge: one copy, one session slot.
  core_->module = LoadLibraryW(L"bubbleplay_app_core.dll");
  if (core_->module) {
    auto get = [&](const char* name) {
      return reinterpret_cast<void*>(GetProcAddress(core_->module, name));
    };
    core_->active = reinterpret_cast<ActiveFn>(get("bp_stream_active"));
    core_->set_frame_callback = reinterpret_cast<SetFrameCallbackFn>(
        get("bp_stream_set_frame_callback"));
    core_->front = reinterpret_cast<FrontFn>(get("bp_stream_front"));
    core_->set_rect = reinterpret_cast<SetRectFn>(get("bp_stream_set_rect"));
    core_->message = reinterpret_cast<MessageFn>(get("bp_stream_message"));
    core_->tick = reinterpret_cast<TickFn>(get("bp_stream_tick"));
  }

  auto* registrar =
      flutter::PluginRegistrarManager::GetInstance()
          ->GetRegistrar<flutter::PluginRegistrarWindows>(
              engine->GetRegistrarForPlugin("BubbleplayStream"));
  textures_ = registrar->texture_registrar();
  channel_ = std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
      registrar->messenger(), "bubbleplay/stream",
      &flutter::StandardMethodCodec::GetInstance());
  channel_->SetMethodCallHandler(
      [this](const auto& call, auto result) {
        HandleCall(call, std::move(result));
      });
  if (core_->ok()) {
    marked_registrar_.store(textures_);
    core_->set_frame_callback(&StreamPlugin::OnFrame);
  }
}

StreamPlugin::~StreamPlugin() {
  if (core_ && core_->ok()) {
    core_->set_frame_callback(nullptr);
  }
  marked_id_.store(-1);
  marked_registrar_.store(nullptr);
  if (channel_) channel_->SetMethodCallHandler(nullptr);
  if (texture_id_ >= 0) {
    KillTimer(window_, kTickTimer);
    textures_->UnregisterTexture(texture_id_);
    texture_id_ = -1;
  }
}

// Any thread: a new picture is ready.
void StreamPlugin::OnFrame() {
  const int64_t id = marked_id_.load();
  auto* textures = marked_registrar_.load();
  if (id >= 0 && textures) textures->MarkTextureFrameAvailable(id);
}

void StreamPlugin::Sync() {
  const bool active = core_->ok() && core_->active();
  if (active && texture_id_ < 0) {
    texture_ = std::make_unique<flutter::TextureVariant>(
        flutter::GpuSurfaceTexture(
            kFlutterDesktopGpuSurfaceTypeDxgiSharedHandle,
            [this](size_t, size_t) -> const FlutterDesktopGpuSurfaceDescriptor* {
              intptr_t handle = 0;
              uint32_t width = 0, height = 0;
              uint64_t serial = 0;
              if (!core_->front(&handle, &width, &height, &serial) || !handle) {
                return nullptr;
              }
              descriptor_ = {};
              descriptor_.struct_size = sizeof(descriptor_);
              descriptor_.handle = reinterpret_cast<void*>(handle);
              descriptor_.width = width;
              descriptor_.height = height;
              descriptor_.visible_width = width;
              descriptor_.visible_height = height;
              descriptor_.format = kFlutterDesktopPixelFormatBGRA8888;
              return &descriptor_;
            }));
    texture_id_ = textures_->RegisterTexture(texture_.get());
    marked_id_.store(texture_id_);
    // The capture's per-frame step (mode switches, cursor clip).
    SetTimer(window_, kTickTimer, kTickMs, nullptr);
  } else if (!active && texture_id_ >= 0) {
    KillTimer(window_, kTickTimer);
    marked_id_.store(-1);
    const int64_t id = texture_id_;
    texture_id_ = -1;
    textures_->UnregisterTexture(id);
    texture_.reset();
  }
}

void StreamPlugin::HandleCall(
    const flutter::MethodCall<flutter::EncodableValue>& call,
    std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result) {
  const std::string& method = call.method_name();
  if (method == "textureId") {
    Sync();
    if (texture_id_ >= 0) {
      result->Success(flutter::EncodableValue(texture_id_));
    } else {
      result->Success();
    }
  } else if (method == "videoRect") {
    if (core_->ok()) {
      if (const auto* map = std::get_if<flutter::EncodableMap>(call.arguments())) {
        core_->set_rect(reinterpret_cast<intptr_t>(window_),
                        static_cast<float>(Number(*map, "x")),
                        static_cast<float>(Number(*map, "y")),
                        static_cast<float>(Number(*map, "width")),
                        static_cast<float>(Number(*map, "height")));
      }
    }
    result->Success();
  } else if (method == "pointer" || method == "key") {
    // Windows captures the keyboard and mouse on the window itself.
    result->Success();
  } else {
    result->NotImplemented();
  }
}

bool StreamPlugin::OnMessage(HWND hwnd, UINT message, WPARAM wparam,
                             LPARAM lparam, LRESULT* result) {
  if (!core_ || !core_->ok()) return false;
  if (message == WM_TIMER && wparam == kTickTimer) {
    Sync();
    core_->tick(reinterpret_cast<intptr_t>(hwnd));
    return true;
  }
  intptr_t value = 0;
  if (core_->message(reinterpret_cast<intptr_t>(hwnd), message,
                     static_cast<uintptr_t>(wparam),
                     static_cast<intptr_t>(lparam), &value)) {
    *result = static_cast<LRESULT>(value);
    return true;
  }
  return false;
}
