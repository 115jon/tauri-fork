// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#[cfg(windows)]
pub mod windows {
  use cef::*;
  use windows::Win32::Foundation::*;
  use windows::Win32::Graphics::{
    Dwm::{DwmDefWindowProc, DwmExtendFrameIntoClientArea},
    Gdi::{ClientToScreen, ScreenToClient},
  };
  use windows::Win32::UI::Controls::MARGINS;
  use windows::Win32::UI::HiDpi::GetDpiForWindow;
  use windows::Win32::UI::WindowsAndMessaging::*;
  use windows::core::{PCWSTR, w};

  /// Same as [WNDPROC] but without the Option wrapper.
  type WindowProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

  const ORIGINAL_WND_PROP: PCWSTR = w!("TAURI_CEF_ORIGINAL_WND_PROC");

  const TITLEBAR_LEADING_WIDTH: i32 = 68;
  const TITLEBAR_HEIGHT: i32 = 28;
  const CAPTION_BUTTON_WIDTH: i32 = 42;
  const ORIGINAL_CHILD_WND_PROP: PCWSTR = w!("TAURI_CEF_ORIGINAL_CHILD_WND_PROC");
  const HOVER_CHILD_WND_PROP: PCWSTR = w!("TAURI_CEF_HOVER_CHILD_WND");
  const RENDERER_MOUSE_LEAVE: u32 = 0x02a3;

  /// Subclasses CEF child windows after they exist so the top-level frame can
  /// receive hit testing for blank titlebar and maximize-button regions.
  pub fn subclass_browser_child_windows(window: &cef::Window) {
    let hwnd = HWND(window.window_handle().0 as _);
    extend_dwm_frame(hwnd);
    unsafe {
      let _ = EnumChildWindows(Some(hwnd), Some(subclass_child_window), LPARAM(0));
    }
  }

  /// Subclasses the top-level CEF window so Windows can perform documented
  /// DWM caption hit testing while client input remains owned by CEF.
  pub fn subclass_window_for_dragging(window: &mut cef::Window) {
    let hwnd = window.window_handle();
    let hwnd = HWND(hwnd.0 as _);
    enable_system_menu(hwnd);
    subclass_window(hwnd, root_window_proc);
    extend_dwm_frame(hwnd);
  }

  fn extend_dwm_frame(hwnd: HWND) {
    let margins = MARGINS {
      cxLeftWidth: 0,
      cxRightWidth: 0,
      cyTopHeight: TITLEBAR_HEIGHT,
      cyBottomHeight: 0,
    };
    unsafe {
      let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
  }

  fn enable_system_menu(hwnd: HWND) {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    if style & WS_SYSMENU.0 as isize != 0 {
      return;
    }

    unsafe {
      let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_SYSMENU.0 as isize);
      let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
      );
    }
  }

  /// Subclasses a window by replacing its window procedure with the given `proc`
  /// and storing the original procedure as a property for later use.
  fn subclass_window(hwnd: HWND, proc: WindowProc) {
    let original_wnd_proc = unsafe { GetPropW(hwnd, ORIGINAL_WND_PROP) };
    if !original_wnd_proc.is_invalid() {
      return;
    }

    unsafe { SetLastError(ERROR_SUCCESS) };

    let original_wnd_proc = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, proc as isize) };
    if original_wnd_proc == 0 && unsafe { GetLastError() } != ERROR_SUCCESS {
      return;
    }

    unsafe {
      let _ = SetPropW(
        hwnd,
        ORIGINAL_WND_PROP,
        Some(HANDLE(original_wnd_proc as _)),
      );
    }
  }

  unsafe extern "system" fn subclass_child_window(
    hwnd: HWND,
    _lparam: LPARAM,
  ) -> windows::core::BOOL {
    let original_wnd_proc = GetPropW(hwnd, ORIGINAL_CHILD_WND_PROP);
    if original_wnd_proc.is_invalid() {
      SetLastError(ERROR_SUCCESS);
      let original_wnd_proc = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, child_window_proc as isize);
      if original_wnd_proc == 0 && GetLastError() != ERROR_SUCCESS {
        return windows::core::BOOL(1);
      }

      let _ = SetPropW(
        hwnd,
        ORIGINAL_CHILD_WND_PROP,
        Some(HANDLE(original_wnd_proc as _)),
      );
    }
    windows::core::BOOL(1)
  }

  unsafe extern "system" fn child_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    if msg == WM_SETCURSOR {
      let mut cursor = POINT::default();
      if GetCursorPos(&mut cursor).is_ok()
        && native_frame_region(hwnd, mouse_lparam(cursor.x, cursor.y))
      {
        if set_native_caption_cursor() {
          return LRESULT(1);
        }
      }
    }

    if msg == WM_NCHITTEST {
      let hit_test = call_original_child_window_proc(hwnd, msg, wparam, lparam);
      if hit_test.0 as i32 == HTCLIENT as i32 && native_frame_region(hwnd, lparam) {
        return LRESULT(HTTRANSPARENT as isize);
      }
      return hit_test;
    }

    call_original_child_window_proc(hwnd, msg, wparam, lparam)
  }

  fn native_frame_region(hwnd: HWND, lparam: LPARAM) -> bool {
    let x = (lparam.0 as i16) as i32;
    let y = ((lparam.0 >> 16) as i16) as i32;
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let root = if root.is_invalid() { hwnd } else { root };
    let mut window_rect = RECT::default();
    if unsafe { GetWindowRect(root, &mut window_rect) }.is_err() {
      return false;
    }

    let dpi = unsafe { GetDpiForWindow(root) }.max(96);
    let leading_width = scaled(TITLEBAR_LEADING_WIDTH, dpi);
    let titlebar_height = scaled(TITLEBAR_HEIGHT, dpi);

    y >= window_rect.top
      && y < window_rect.top + titlebar_height
      && x >= window_rect.left + leading_width
      && x < window_rect.right
  }

  fn mouse_lparam(x: i32, y: i32) -> LPARAM {
    let packed = ((x as u32 & 0xffff) | ((y as u32 & 0xffff) << 16)) as i32 as isize;
    LPARAM(packed)
  }

  fn set_native_caption_cursor() -> bool {
    unsafe {
      let Ok(arrow) = LoadCursorW(None, IDC_ARROW) else {
        return false;
      };
      SetCursor(Some(arrow));
    }
    true
  }

  fn is_native_caption_hit_test(hit_test: i32) -> bool {
    hit_test == HTCAPTION as i32
      || hit_test == HTMINBUTTON as i32
      || hit_test == HTMAXBUTTON as i32
      || hit_test == HTCLOSE as i32
  }

  #[cfg(test)]
  mod tests {
    use super::mouse_lparam;

    #[test]
    fn mouse_lparam_preserves_signed_screen_coordinates() {
      assert_eq!(mouse_lparam(-12, 640).0 as u32, 0x0280fff4);
    }
  }

  unsafe fn call_original_child_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    let original_wnd_proc = GetPropW(hwnd, ORIGINAL_CHILD_WND_PROP);
    let original_wnd_proc = std::mem::transmute::<_, WindowProc>(original_wnd_proc.0);
    CallWindowProcW(Some(original_wnd_proc), hwnd, msg, wparam, lparam)
  }

  unsafe extern "system" fn root_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    if msg == WM_ACTIVATE {
      extend_dwm_frame(hwnd);
    }

    if msg == WM_NCLBUTTONUP && wparam.0 == HTMAXBUTTON as usize {
      if let Some(result) = handle_dwm_message(hwnd, msg, wparam, lparam) {
        return result;
      }
      activate_maximize_button(hwnd);
      return LRESULT(0);
    }

    if msg == WM_SETCURSOR && is_native_caption_hit_test((lparam.0 as u16) as i32) {
      if set_native_caption_cursor() {
        return LRESULT(1);
      }
    }

    if msg == WM_NCMOUSEMOVE && is_native_caption_hit_test(wparam.0 as i32) {
      forward_native_mouse_move(hwnd, lparam);
      let _ = set_native_caption_cursor();
    } else if msg == WM_NCMOUSELEAVE {
      clear_native_mouse_hover(hwnd);
    }

    if msg == WM_NCHITTEST {
      if let Some(result) = handle_dwm_message(hwnd, msg, wparam, lparam) {
        let hit_test = result.0 as i32;
        if hit_test == HTMAXBUTTON as i32
          || hit_test == HTCAPTION as i32
          || hit_test == HTMINBUTTON as i32
          || hit_test == HTCLOSE as i32
        {
          return result;
        }

        if hit_test != HTCLIENT as i32 {
          return result;
        }
      }

      let hit_test = custom_caption_hit_test(hwnd, lparam);
      if hit_test != HTCLIENT as i32 {
        return LRESULT(hit_test as isize);
      }
    } else if let Some(result) = handle_dwm_message(hwnd, msg, wparam, lparam) {
      return result;
    }

    if msg == WM_NCRBUTTONDOWN && wparam.0 == HTCAPTION as usize {
      return LRESULT(0);
    }

    if msg == WM_NCRBUTTONUP && wparam.0 == HTCAPTION as usize {
      return show_system_menu(hwnd, lparam);
    }

    if msg == WM_NCLBUTTONDOWN {
      return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    if is_native_caption_message(msg) {
      return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    call_original_window_proc(hwnd, msg, wparam, lparam)
  }

  fn forward_native_mouse_move(hwnd: HWND, lparam: LPARAM) {
    let point = POINT {
      x: (lparam.0 as i16) as i32,
      y: ((lparam.0 >> 16) as i16) as i32,
    };
    let Some(child) = find_native_hover_child(hwnd) else {
      clear_native_mouse_hover(hwnd);
      return;
    };

    let mut child_point = point;
    if !unsafe { ScreenToClient(child, &mut child_point) }.as_bool() {
      return;
    }

    unsafe {
      let previous_child = GetPropW(hwnd, HOVER_CHILD_WND_PROP);
      if !previous_child.is_invalid() && previous_child.0 != child.0 as _ {
        let previous_child = HWND(previous_child.0 as _);
        if IsWindow(Some(previous_child)).as_bool() {
          let _ = SendMessageW(
            previous_child,
            RENDERER_MOUSE_LEAVE,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
          );
        }
      }
      let _ = SetPropW(hwnd, HOVER_CHILD_WND_PROP, Some(HANDLE(child.0 as _)));
      let _ = SendMessageW(
        child,
        WM_MOUSEMOVE,
        Some(WPARAM(0)),
        Some(mouse_lparam(child_point.x, child_point.y)),
      );
    }
  }

  fn find_native_hover_child(hwnd: HWND) -> Option<HWND> {
    let mut child = unsafe { GetWindow(hwnd, GW_CHILD).ok()? };
    while !child.is_invalid() {
      if unsafe { GetPropW(child, ORIGINAL_CHILD_WND_PROP) }.is_invalid() {
        if let Some(descendant) = find_native_hover_child(child) {
          return Some(descendant);
        }
      } else {
        return Some(child);
      }

      child = unsafe { GetWindow(child, GW_HWNDNEXT).ok()? };
    }

    None
  }

  fn clear_native_mouse_hover(hwnd: HWND) {
    let child = unsafe { GetPropW(hwnd, HOVER_CHILD_WND_PROP) };
    if child.is_invalid() {
      return;
    }

    unsafe {
      let child = HWND(child.0 as _);
      if IsWindow(Some(child)).as_bool() {
        let _ = SendMessageW(
          child,
          RENDERER_MOUSE_LEAVE,
          Some(WPARAM(0)),
          Some(LPARAM(0)),
        );
      }
      let _ = RemovePropW(hwnd, HOVER_CHILD_WND_PROP);
    }
  }

  fn handle_dwm_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !matches!(
      msg,
      WM_NCHITTEST
        | WM_NCMOUSEMOVE
        | WM_NCMOUSELEAVE
        | WM_NCLBUTTONDOWN
        | WM_NCLBUTTONUP
        | WM_NCLBUTTONDBLCLK
    ) {
      return None;
    }

    let mut dwm_result = LRESULT(0);
    if unsafe { DwmDefWindowProc(hwnd, msg, wparam, lparam, &mut dwm_result) }.as_bool() {
      Some(dwm_result)
    } else {
      extend_dwm_frame(hwnd);
      if unsafe { DwmDefWindowProc(hwnd, msg, wparam, lparam, &mut dwm_result) }.as_bool() {
        Some(dwm_result)
      } else {
        None
      }
    }
  }

  fn custom_caption_hit_test(hwnd: HWND, lparam: LPARAM) -> i32 {
    let x = (lparam.0 as i16) as i32;
    let y = ((lparam.0 >> 16) as i16) as i32;
    let mut client_rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut client_rect) }.is_err() {
      return HTCLIENT as i32;
    }

    let mut client_origin = POINT { x: 0, y: 0 };
    if !unsafe { ClientToScreen(hwnd, &mut client_origin) }.as_bool() {
      return HTCLIENT as i32;
    }

    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let leading_width = scaled(TITLEBAR_LEADING_WIDTH, dpi);
    let titlebar_height = scaled(TITLEBAR_HEIGHT, dpi);
    let button_width = scaled(CAPTION_BUTTON_WIDTH, dpi);
    let right = client_origin.x + client_rect.right;
    let top = client_origin.y;

    if y < top || y >= top + titlebar_height || x < client_origin.x || x >= right {
      return HTCLIENT as i32;
    }

    let distance_from_right = right - x;
    if distance_from_right < button_width
      || (distance_from_right >= button_width * 2 && distance_from_right < button_width * 3)
      || x < client_origin.x + leading_width
    {
      return HTCLIENT as i32;
    }

    if distance_from_right < button_width * 2 {
      return HTMAXBUTTON as i32;
    }

    HTCAPTION as i32
  }

  fn scaled(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 95) / 96
  }

  fn is_native_caption_message(msg: u32) -> bool {
    matches!(
      msg,
      WM_NCLBUTTONDOWN | WM_NCLBUTTONUP | WM_NCLBUTTONDBLCLK | WM_NCRBUTTONDBLCLK | WM_NCRBUTTONUP
    )
  }

  fn show_system_menu(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let menu = unsafe { GetSystemMenu(hwnd, false) };
    if menu.is_invalid() {
      return LRESULT(0);
    }

    let x = (lparam.0 as i16) as i32;
    let y = ((lparam.0 >> 16) as i16) as i32;
    unsafe {
      let _ = SetForegroundWindow(hwnd);
    }
    let command = unsafe {
      TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        x,
        y,
        None,
        hwnd,
        None,
      )
    };

    if command.0 != 0 {
      unsafe {
        let _ = PostMessageW(
          Some(hwnd),
          WM_SYSCOMMAND,
          WPARAM(command.0 as usize),
          LPARAM(0),
        );
      }
    }

    LRESULT(0)
  }

  fn activate_maximize_button(hwnd: HWND) {
    unsafe {
      let command = if IsZoomed(hwnd).as_bool() {
        SW_RESTORE
      } else {
        SW_MAXIMIZE
      };
      let _ = ShowWindow(hwnd, command);
    }
  }

  unsafe fn call_original_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    let original_wnd_proc = GetPropW(hwnd, ORIGINAL_WND_PROP);
    let original_wnd_proc = std::mem::transmute::<_, WindowProc>(original_wnd_proc.0);
    unsafe { CallWindowProcW(Some(original_wnd_proc), hwnd, msg, wparam, lparam) }
  }
}
