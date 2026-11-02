#ifndef RUNNER_STREAM_PLUGIN_H_
#define RUNNER_STREAM_PLUGIN_H_

#include <flutter/flutter_engine.h>
#include <flutter/method_channel.h>
#include <flutter/standard_method_codec.h>
#include <flutter/texture_registrar.h>
#include <windows.h>

#include <atomic>
#include <memory>

// The Windows side of the session video, channel `bubbleplay/stream`.
//
// The Rust core (bubbleplay_app_core.dll) decodes the stream and draws every picture into a
// shared D3D11 texture. This plugin hands that texture to the engine as a GPU surface texture
// (DXGI shared handle) and tells the engine when a new picture is ready. It also gives the
// core the window's messages, so the keyboard, mouse and Raw Input are captured natively, and
// the rectangle the video occupies. No pixel passes through Dart.
class StreamPlugin {
 public:
  StreamPlugin(flutter::FlutterEngine* engine, HWND window);
  ~StreamPlugin();

  StreamPlugin(const StreamPlugin&) = delete;
  StreamPlugin& operator=(const StreamPlugin&) = delete;

  // Offers a window message to the input capture. Returns true when it was
  // handled; |result| is then the window procedure's result.
  bool OnMessage(HWND hwnd, UINT message, WPARAM wparam, LPARAM lparam,
                 LRESULT* result);

 private:
  struct Core;  // The functions resolved from bubbleplay_app_core.dll.

  void HandleCall(const flutter::MethodCall<flutter::EncodableValue>& call,
                  std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result);
  // Registers the texture while a session runs, unregisters it when it ends.
  void Sync();
  static void OnFrame();

  HWND window_;
  flutter::TextureRegistrar* textures_ = nullptr;
  std::unique_ptr<flutter::MethodChannel<flutter::EncodableValue>> channel_;
  std::unique_ptr<Core> core_;
  std::unique_ptr<flutter::TextureVariant> texture_;
  FlutterDesktopGpuSurfaceDescriptor descriptor_{};
  int64_t texture_id_ = -1;
  // The id the frame callback may mark, or -1. Written on the platform thread only.
  static std::atomic<int64_t> marked_id_;
  static std::atomic<flutter::TextureRegistrar*> marked_registrar_;
};

#endif  // RUNNER_STREAM_PLUGIN_H_
