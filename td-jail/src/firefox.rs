//! Bounded Firefox autotest introspection over Marionette's loopback protocol.

use crate::cgroup::ProcessSandbox;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

const MARIONETTE_PORT: u16 = 2828;
const PROBE_DEADLINE: Duration = Duration::from_secs(60);
const NETWORK_PROBE_DEADLINE: Duration = Duration::from_secs(60);
const DOWNLOAD_PROBE_DEADLINE: Duration = Duration::from_secs(40);
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_HEADER_DIGITS: usize = 7;
const HELLO: &str = r#"{"applicationType":"gecko","marionetteProtocol":3}"#;
const NEW_SESSION: &str = r#"[0,1,"WebDriver:NewSession",{}]"#;
const NEW_SESSION_PREFIX: &str = r#"[1,1,null,{"sessionId":"#;
const EXECUTE_RESPONSE_PREFIX: &str = "[1,3,null,{\"value\":\"";
const EXECUTE_RESPONSE_SUFFIX: &str = r#""}]"#;
const REPORT_PREFIX: &str = "TD-FIREFOX-SUPPORT-V1";
const INPUT_CONTENT_ARMED: &str = "TD-FIREFOX-INPUT-CONTENT-ARMED";
const INPUT_CHROME_ARMED: &str = "TD-FIREFOX-INPUT-CHROME-ARMED";
const INPUT_CONTENT_OK: &str = "TD-FIREFOX-INPUT-CONTENT-OK";
const INPUT_MENU_OK: &str = "TD-FIREFOX-INPUT-MENU-OK";
const INPUT_FINAL_OK: &str = "TD-FIREFOX-INPUT-FINAL-OK";
const INPUT_CLIPBOARD_REFOCUS_ARMED: &str = "TD-FIREFOX-CLIPBOARD-REFOCUS-ARMED";
const INPUT_CLIPBOARD_WINDOW_ARMED: &str = "TD-FIREFOX-CLIPBOARD-WINDOW-ARMED";
const INPUT_CLIPBOARD_ARMED: &str = "TD-FIREFOX-CLIPBOARD-CHROME-ARMED";
const INPUT_CLIPBOARD_RETRY: &str = "TD-FIREFOX-CLIPBOARD-RETRY";
const INPUT_CLIPBOARD_PUBLIC_ARMED: &str = "TD-FIREFOX-CLIPBOARD-ARMED";
const INPUT_CLIPBOARD_PUBLIC_RETRY: &str = "TD-FIREFOX-CLIPBOARD-RETRY-ARMED";
const INPUT_CLIPBOARD_OK: &str = "TD-FIREFOX-CLIPBOARD-OK";
const INPUT_DOWNLOAD_ARMED: &str = "TD-FIREFOX-DOWNLOAD-CONTENT-ARMED";
const INPUT_DOWNLOAD_PUBLIC_ARMED: &str = "TD-FIREFOX-DOWNLOAD-ARMED";
const INPUT_DOWNLOAD_CLICKED: &str = "TD-FIREFOX-DOWNLOAD-CLICKED";
const INPUT_FILE_CHOOSER_REFOCUS_ARMED: &str = "TD-FIREFOX-FILE-CHOOSER-REFOCUS-CONTENT-ARMED";
const INPUT_FILE_CHOOSER_PUBLIC_REFOCUS_ARMED: &str = "TD-FIREFOX-FILE-CHOOSER-REFOCUS-ARMED";
const INPUT_FILE_CHOOSER_ARMED: &str = "TD-FIREFOX-FILE-CHOOSER-CONTENT-ARMED";
const INPUT_FILE_CHOOSER_PUBLIC_ARMED: &str = "TD-FIREFOX-FILE-CHOOSER-ARMED";
const INPUT_FILE_CHOOSER_FOCUSED: &str = "TD-FIREFOX-FILE-CHOOSER-CONTENT-FOCUSED";
const INPUT_FILE_CHOOSER_PUBLIC_FOCUSED: &str = "TD-FIREFOX-FILE-CHOOSER-FOCUSED";
const INPUT_FILE_CHOOSER_OK: &str = "TD-FIREFOX-FILE-CHOOSER-CONTENT-OK";
const INPUT_FILE_CHOOSER_PUBLIC_OK: &str = "TD-FIREFOX-FILE-CHOOSER-OK bytes=23";
const FIREFOX_NETWORK_TEST_URL: &str = "https://git.kernel.org/";
const FIREFOX_NETWORK_CONTENT_OK: &str = "TD-FIREFOX-NETWORK-CONTENT-OK";
pub(crate) const FIREFOX_NETWORK_RUNTIME_MARKER: &str =
    "TD-FIREFOX-NETWORK-HTTPS-OK";
const DOWNLOAD_DIRECTORY: &str = "/var/home/tester/Downloads";
const DOWNLOAD_NAME: &str = "td-firefox-download.txt";
const DOWNLOAD_BYTES: &[u8] = b"TD-FIREFOX-DOWNLOAD-V1\n";
const DOWNLOAD_UID: u32 = 1000;
const DOWNLOAD_GID: u32 = 1000;
const MAX_DOWNLOAD_DIRECTORY_ENTRIES: usize = 64;

const NETWORK_DOCUMENT_SCRIPT_TEMPLATE: &str = r#"
const done = arguments[arguments.length - 1];
try {
  const expected = new URL(__TD_EXPECTED_URL__);
  const url = new URL(document.location.href);
  const documentUrl = new URL(document.documentURI);
  const navigation = performance.getEntriesByType("navigation")[0];
  const body = document.body;
  const text = body ? String(body.innerText || body.textContent || "").slice(0, 4097) : "";
  if (document.readyState !== "complete") {
    done("TD-FIREFOX-NETWORK-ERROR:not-complete");
  } else if (url.origin !== expected.origin || documentUrl.origin !== expected.origin) {
    done("TD-FIREFOX-NETWORK-ERROR:wrong-origin");
  } else if (!navigation || Number(navigation.responseStatus) !== 200) {
    done("TD-FIREFOX-NETWORK-ERROR:http-status");
  } else if (!body || !text.trim()) {
    done("TD-FIREFOX-NETWORK-ERROR:empty-document");
  } else {
    done("TD-FIREFOX-NETWORK-CONTENT-OK");
  }
} catch (error) {
  done("TD-FIREFOX-NETWORK-ERROR:" +
    String(error).replace(/[|,"\\\r\n]/g, "_").slice(0, 256));
}
"#;

fn network_document_script() -> String {
    NETWORK_DOCUMENT_SCRIPT_TEMPLATE.replace(
        "__TD_EXPECTED_URL__",
        &json_string(FIREFOX_NETWORK_TEST_URL),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputStage {
    Arm,
    Menu,
    Final,
    ClipboardRefocusArm,
    ClipboardRefocus,
    Clipboard,
    Download,
    FileChooser,
    FileChooserFocus,
    FileChooserResult,
}

impl InputStage {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "arm" => Some(Self::Arm),
            "menu" => Some(Self::Menu),
            "final" => Some(Self::Final),
            "clipboard-refocus-arm" => Some(Self::ClipboardRefocusArm),
            "clipboard-refocus" => Some(Self::ClipboardRefocus),
            "clipboard" => Some(Self::Clipboard),
            "download" => Some(Self::Download),
            "file-chooser" => Some(Self::FileChooser),
            "file-chooser-focus" => Some(Self::FileChooserFocus),
            "file-chooser-result" => Some(Self::FileChooserResult),
            _ => None,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Arm => "TD-FIREFOX-INPUT-ARMED",
            Self::Menu => "TD-FIREFOX-INPUT-MENU",
            Self::Final => "TD-FIREFOX-INPUT-OK",
            Self::ClipboardRefocusArm => "TD-FIREFOX-CLIPBOARD-REFOCUS-ARMED",
            Self::ClipboardRefocus => "TD-FIREFOX-CLIPBOARD-WINDOW-ARMED",
            Self::Clipboard => "TD-FIREFOX-CLIPBOARD-OK",
            Self::Download => "TD-FIREFOX-DOWNLOAD-CLICKED",
            Self::FileChooser => INPUT_FILE_CHOOSER_PUBLIC_ARMED,
            Self::FileChooserFocus => INPUT_FILE_CHOOSER_PUBLIC_FOCUSED,
            Self::FileChooserResult => INPUT_FILE_CHOOSER_PUBLIC_OK,
        }
    }
}

const CONTENT_ARM_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const input = document.getElementById("td-input");
if (!input) {
  done("TD-FIREFOX-INPUT-ERROR:no-input");
} else {
  const state = {
    moves: 0, inputs: 0, wheels: 0, contexts: 0,
    audio: { starts: 0, ended: 0, closed: 0, rate: 0, error: "" }
  };
  window.__tdPhysicalInput = state;
  document.addEventListener("mousemove", () => state.moves++);
  input.addEventListener("input", () => state.inputs++);
  input.addEventListener("keydown", event => {
    const audio = state.audio;
    if (!event.isTrusted || audio.starts !== 0) return;
    audio.starts++;
    try {
      const context = new AudioContext({ sampleRate: 48000 });
      const oscillator = context.createOscillator();
      const gain = context.createGain();
      audio.rate = context.sampleRate;
      audio.context = context;
      audio.oscillator = oscillator;
      oscillator.frequency.value = 440;
      gain.gain.value = 0.25;
      oscillator.connect(gain).connect(context.destination);
      oscillator.addEventListener("ended", () => {
        audio.ended++;
        context.close().then(() => audio.closed++).catch(error => {
          audio.error = String(error).slice(0, 128);
        });
      }, { once: true });
      oscillator.start();
      oscillator.stop(context.currentTime + 1);
    } catch (error) {
      audio.error = String(error).slice(0, 128);
    }
  }, { capture: true });
  document.addEventListener("wheel", event => {
    if (event.deltaY > 0) state.wheels++;
  }, { passive: true });
  document.addEventListener("contextmenu", () => state.contexts++);
  input.value = "";
  input.focus();
  done("TD-FIREFOX-INPUT-CONTENT-ARMED");
}
"#;

const CHROME_ARM_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const win = Services.wm.getMostRecentWindow("navigator:browser");
const popup = win && win.document.getElementById("contentAreaContextMenu");
if (!popup) {
  done("TD-FIREFOX-INPUT-ERROR:no-context-menu");
} else {
  const state = { shown: 0, hidden: 0 };
  win.__tdPhysicalInput = state;
  popup.addEventListener("popupshown", () => state.shown++, { once: true });
  popup.addEventListener("popuphidden", () => state.hidden++, { once: true });
  done("TD-FIREFOX-INPUT-CHROME-ARMED");
}
"#;

const CONTENT_MENU_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const state = window.__tdPhysicalInput;
  const input = document.getElementById("td-input");
  const audio = state && state.audio;
  const typed = input && input.value.length > 0 && input.value.length <= 4 &&
    Array.from(input.value).every(value => value === "x");
  const ok = state && input && state.moves > 0 && state.inputs > 0 &&
    state.wheels > 0 && state.contexts > 0 && typed &&
    window.scrollY > 0 && audio && audio.starts === 1 &&
    audio.ended === 1 && audio.closed === 1 && audio.rate === 48000 &&
    audio.error === "";
  if (!ok && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  const detail = state ? [state.moves, state.inputs, state.wheels,
    state.contexts, input && input.value, window.scrollY,
    audio && audio.starts, audio && audio.ended, audio && audio.closed,
    audio && audio.rate, audio && audio.error].join(",") :
    "no-state";
  done(ok ? "TD-FIREFOX-INPUT-CONTENT-OK" :
    "TD-FIREFOX-INPUT-ERROR:content:" + detail);
};
check();
"#;

const CHROME_MENU_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const win = Services.wm.getMostRecentWindow("navigator:browser");
  const popup = win && win.document.getElementById("contentAreaContextMenu");
  const state = win && win.__tdPhysicalInput;
  const ok = state && state.shown > 0 && popup && popup.state === "open";
  if (!ok && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  done(ok ? "TD-FIREFOX-INPUT-MENU-OK" :
    "TD-FIREFOX-INPUT-ERROR:menu:" + (state ? state.shown + "," +
    (popup && popup.state) : "no-state"));
};
check();
"#;

const CHROME_FINAL_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const win = Services.wm.getMostRecentWindow("navigator:browser");
  const popup = win && win.document.getElementById("contentAreaContextMenu");
  const state = win && win.__tdPhysicalInput;
  const ok = state && state.shown > 0 && state.hidden > 0 && popup &&
    popup.state === "closed";
  if (!ok && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  done(ok ? "TD-FIREFOX-INPUT-FINAL-OK" :
    "TD-FIREFOX-INPUT-ERROR:final:" + (state ? state.shown + "," +
    state.hidden + "," + (popup && popup.state) : "no-state"));
};
check();
"#;

const CHROME_CLIPBOARD_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const win = Services.wm.getMostRecentWindow("navigator:browser");
  const state = win && win.__tdClipboardPaste;
  const value = win && win.gURLBar && win.gURLBar.value;
  const firstBounded = state && state.commandEnds === 1 &&
    state.retryFloor === 0 &&
    state.shortcuts >= 1 && state.shortcuts <= 4;
  const retryEvents = state && state.shortcuts - state.retryFloor;
  const secondBounded = state && state.commandEnds === 2 &&
    state.retryFloor >= 1 &&
    state.retryFloor <= 4 && retryEvents >= 1 && retryEvents <= 4;
  const bounded = (firstBounded || secondBounded) &&
    state.pastes === state.shortcuts;
  const accounted = bounded &&
    state.emptyPastes + state.exactPastes === state.pastes;
  const valueRepeats = value && value.length % "Welcome".length === 0 &&
    value === "Welcome".repeat(value.length / "Welcome".length) ?
    value.length / "Welcome".length : 0;
  const pasted = accounted && valueRepeats >= 1 &&
    valueRepeats <= state.pastes && state.exactPastes <= valueRepeats &&
    !state.unexpected;
  const retry = state && state.commandEnds === 1 &&
    state.retryFloor === 0 &&
    state.shortcuts >= 1 && state.shortcuts <= 4 &&
    state.pastes === state.shortcuts && state.emptyPastes === state.pastes &&
    state.exactPastes === 0 && !state.unexpected && value === state.initial;
  if (!pasted && !retry && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  if (retry) state.retryFloor = state.shortcuts;
  done(pasted ? "TD-FIREFOX-CLIPBOARD-OK" : retry ?
    "TD-FIREFOX-CLIPBOARD-RETRY" :
    "TD-FIREFOX-INPUT-ERROR:clipboard:" +
    [state && state.commandEnds, state && state.retryFloor,
      state && state.shortcuts, state && state.pastes,
      state && state.emptyPastes, state && state.exactPastes,
      state && state.text, state && state.unexpected,
      win && win.gURLBar && win.gURLBar.focused,
      win && win.document.activeElement && win.document.activeElement.id,
      value].map(String).join(":"));
};
check();
"#;

const CONTENT_CLIPBOARD_REFOCUS_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const state = window.__tdClipboardRefocus;
  const refocused = state && state.down === 1;
  if (!refocused && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  done(refocused ? "TD-FIREFOX-CLIPBOARD-WINDOW-ARMED" :
    "TD-FIREFOX-INPUT-ERROR:clipboard-refocus");
};
check();
"#;

const CONTENT_CLIPBOARD_REFOCUS_ARM_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const state = { down: 0 };
window.__tdClipboardRefocus = state;
window.addEventListener("mousedown", () => state.down++, {
  capture: true, once: true
});
done("TD-FIREFOX-CLIPBOARD-REFOCUS-ARMED");
"#;

const CHROME_CLIPBOARD_ARM_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const win = Services.wm.getMostRecentWindow("navigator:browser");
  const urlbar = win && win.gURLBar;
  const focused = urlbar && urlbar.focused;
  const field = urlbar && urlbar.inputField;
  const selected = field && field.selectionStart === 0 &&
    field.selectionEnd === urlbar.value.length;
  const active = win && win.document.activeElement;
  const detail = [Boolean(win), Boolean(urlbar), Boolean(focused),
    Boolean(field && field === active), Boolean(selected),
    Boolean(win && win.document.hasFocus()),
    Boolean(Services.focus.activeWindow === win),
    Boolean(urlbar && urlbar.hasAttribute("focused"))].join(",");
  if ((!focused || !selected) && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  if (!focused || !selected) {
    done("TD-FIREFOX-INPUT-ERROR:clipboard-arm:" + detail);
    return;
  }
  const state = { commandEnds: 0, shortcuts: 0, pastes: 0, retryFloor: 0,
    emptyPastes: 0, exactPastes: 0, text: "", unexpected: false,
    initial: urlbar.value };
  win.__tdClipboardPaste = state;
  win.addEventListener("keydown", event => {
    if (event.ctrlKey && event.key === "v") state.shortcuts++;
  }, { capture: true });
  win.addEventListener("keyup", event => {
    if (event.key === "Shift" && !event.ctrlKey && !event.altKey &&
        !event.metaKey) state.commandEnds++;
  }, { capture: true });
  win.addEventListener("paste", event => {
    state.pastes++;
    const text = event.clipboardData ?
      event.clipboardData.getData("text/plain") : "no-data";
    state.text = text;
    if (text === "") state.emptyPastes++;
    else if (text === "Welcome") state.exactPastes++;
    else state.unexpected = true;
  }, { capture: true });
  done("TD-FIREFOX-CLIPBOARD-CHROME-ARMED");
};
check();
"#;

const CONTENT_DOWNLOAD_ARM_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const link = document.getElementById("td-download");
const expected = "https://localhost:8443/download.txt";
if (!link || link.href !== expected ||
    link.download !== "td-firefox-download.txt") {
  done("TD-FIREFOX-INPUT-ERROR:download-link");
} else {
  const state = { keys: 0, clicks: 0, commandEnds: 0, trusted: true };
  window.__tdDownload = state;
  link.addEventListener("keydown", event => {
    if (event.key === "Enter") {
      state.keys++;
      if (state.keys > 1) event.preventDefault();
      if (!event.isTrusted) state.trusted = false;
    }
  }, { capture: true });
  link.addEventListener("click", event => {
    state.clicks++;
    if (!event.isTrusted) state.trusted = false;
  }, { capture: true });
  window.addEventListener("keyup", event => {
    if (event.key === "Shift" && !event.repeat) state.commandEnds++;
    if (event.key === "Shift" && !event.isTrusted) state.trusted = false;
  }, { capture: true });
  link.focus();
  done(document.hasFocus() && document.activeElement === link ?
    "TD-FIREFOX-DOWNLOAD-CONTENT-ARMED" :
    "TD-FIREFOX-INPUT-ERROR:download-focus");
}
"#;

const CONTENT_DOWNLOAD_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const state = window.__tdDownload;
  const ok = state && state.keys >= 1 && state.keys <= 4 &&
    state.clicks === 1 && state.commandEnds === 1 && state.trusted;
  if (!ok && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  done(ok ? "TD-FIREFOX-DOWNLOAD-CLICKED" :
    "TD-FIREFOX-INPUT-ERROR:download:" +
    [state && state.keys, state && state.clicks, state && state.commandEnds,
      state && state.trusted].map(String).join(":"));
};
check();
"#;

const CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const focus = document.createElement("button");
focus.id = "td-upload-focus";
focus.type = "button";
focus.textContent = "Focus Firefox for its native Open File command";
document.body.appendChild(focus);
focus.style.position = "fixed";
focus.style.inset = "0";
focus.style.width = "100%";
focus.style.height = "100%";
focus.style.zIndex = "2147483647";
const state =
  { downs: 0, clicks: 0, trusted: true, button: null, x: null, y: null };
window.__tdFileChooserRefocus = state;
focus.addEventListener("mousedown", event => {
  state.downs++;
  if (!event.isTrusted) state.trusted = false;
}, { capture: true, once: true });
focus.addEventListener("click", event => {
  state.clicks++;
  state.button = event.button;
  state.x = event.clientX;
  state.y = event.clientY;
  if (!event.isTrusted) state.trusted = false;
}, { capture: true, once: true });
const rect = focus.getBoundingClientRect();
if (rect.left > 0 || rect.top > 0 || rect.right < innerWidth ||
    rect.bottom < innerHeight) {
  done("TD-FIREFOX-INPUT-ERROR:file-chooser-layout");
  return;
}
done("TD-FIREFOX-FILE-CHOOSER-REFOCUS-CONTENT-ARMED");
"#;

const CONTENT_FILE_CHOOSER_ARM_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const refocus = window.__tdFileChooserRefocus;
  if ((!refocus || refocus.downs !== 1 || refocus.clicks !== 1) &&
      Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  const focus = document.getElementById("td-upload-focus");
  if (!refocus || refocus.downs !== 1 || refocus.clicks !== 1 ||
      refocus.button !== 0 || !refocus.trusted ||
      !Number.isFinite(refocus.x) || !Number.isFinite(refocus.y) ||
      !focus || !document.hasFocus() ||
      document.activeElement !== focus) {
    done("TD-FIREFOX-INPUT-ERROR:file-chooser-input");
    return;
  }
  focus.style.inset = "auto";
  focus.style.left = "0";
  focus.style.top = "0";
  focus.style.width = "1px";
  focus.style.height = "1px";
  requestAnimationFrame(() => requestAnimationFrame(() => {
    done("TD-FIREFOX-FILE-CHOOSER-CONTENT-ARMED");
  }));
};
check();
"#;

const CONTENT_FILE_CHOOSER_FOCUS_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = () => {
  const state = window.__tdFileChooserRefocus;
  const ok = state && state.downs === 1 && state.clicks === 1 &&
    state.button === 0 && state.trusted && Number.isFinite(state.x) &&
    Number.isFinite(state.y) && document.hasFocus();
  if (!ok && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  done(ok ? "TD-FIREFOX-FILE-CHOOSER-CONTENT-FOCUSED" :
    "TD-FIREFOX-INPUT-ERROR:file-chooser-focus:" +
    [state && state.downs, state && state.clicks, state && state.button,
      state && state.trusted, state && state.x, state && state.y,
      document.hasFocus()].map(String).join(":"));
};
check();
"#;

const CONTENT_FILE_CHOOSER_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
const expires = Date.now() + 20000;
const check = async () => {
  const text = document.body && document.body.textContent;
  const ok = location.href ===
      "file:///home/td/Downloads/td-firefox-download.txt" &&
    document.contentType === "text/plain" &&
    text === "TD-FIREFOX-DOWNLOAD-V1\n";
  if (!ok && Date.now() < expires) {
    setTimeout(check, 50);
    return;
  }
  done(ok ? "TD-FIREFOX-FILE-CHOOSER-CONTENT-OK" :
    "TD-FIREFOX-INPUT-ERROR:file-chooser:" +
    [location.href, document.contentType, text].map(String).join(":"));
};
check();
"#;

// This runs only after the exact QEMU autotest launch enabled Marionette and
// opted into privileged script execution. It reads Firefox's own about:support
// providers and reports Firefox's own child-role mapping. The Rust side binds
// those namespace PIDs to the live application cgroup and reads their kernel
// status independently. The returned alphabet deliberately needs no JSON
// escapes, so the Rust side can validate one exact response shape.
const REPORT_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
(async () => {
  const { Troubleshoot } = ChromeUtils.importESModule(
    "resource://gre/modules/Troubleshoot.sys.mjs"
  );
  const snapshot = await Troubleshoot.snapshot();
  const graphics = snapshot.graphics || {};
  const sandbox = snapshot.sandbox || {};
  const processes = snapshot.processes || {};
  const remoteTypes = processes.remoteTypes || {};
  const processInfo = await ChromeUtils.requestProcInfo();
  const window = Services.wm.getMostRecentWindow("");
  const roles = new Map([
    ["content", new Set()],
    ["gpu", new Set()],
    ["socket", new Set()],
    ["rdd", new Set()],
    ["utility", new Set()],
  ]);
  const mediaUtilityPids = new Set();
  const contentTypes = new Set([
    "web",
    "webIsolated",
    "file",
    "extension",
    "privilegedabout",
    "privilegedmozilla",
    "withCoopCoep",
    "webServiceWorker",
    "preallocated",
    "inference",
  ]);
  const mediaUtilityActors = new Set([
    "audioDecoder_Generic",
    "audioDecoder_AppleMedia",
    "audioDecoder_WMF",
    "mfMediaEngineCDM",
  ]);
  const nonMediaUtilityActors = new Set([
    "jSOracle",
    "windowsUtils",
    "windowsFileDialog",
    "pkcs11Module",
  ]);
  const add = (role, pid) => {
    const set = roles.get(role);
    if (set && Number.isInteger(pid) && pid > 0) {
      set.add(pid);
    }
  };
  for (const child of processInfo.children) {
    const type = String(child.type || "");
    if (contentTypes.has(type)) {
      add("content", child.pid);
    } else if (type === "gpu" || type === "vr") {
      add("gpu", child.pid);
    } else if (type === "socket") {
      add("socket", child.pid);
    } else if (type === "rdd") {
      add("rdd", child.pid);
    } else if (type === "utility") {
      add("utility", child.pid);
      const actors = Array.from(child.utilityActors || []);
      for (const actor of actors) {
        const actorName = String(actor.actorName || "");
        if (mediaUtilityActors.has(actorName)) {
          mediaUtilityPids.add(child.pid);
        } else if (!nonMediaUtilityActors.has(actorName)) {
          throw new Error(`unknown utility actor ${actorName}`);
        }
      }
    } else if (type === "gmpPlugin") {
      add("utility", child.pid);
      mediaUtilityPids.add(child.pid);
    } else if (type !== "forkServer") {
      throw new Error(`unknown process type ${type}`);
    }
  }
  try {
    add("gpu", window.windowUtils.gpuProcessPid);
  } catch (_) {}
  try {
    if (Services.io.socketProcessLaunched) {
      add("socket", Services.io.socketProcessId);
    }
  } catch (_) {}
  const roleReports = [];
  for (const [role, pids] of roles) {
    roleReports.push(`${role}:${Array.from(pids).sort((a, b) => a - b).join(".")}`);
  }
  const clean = value => {
    const sanitized = Array.from(
      String(value === undefined ? "missing" : value),
      character => {
        const code = character.codePointAt(0);
        return code >= 0x20 && code <= 0x7e &&
          !"|,\"\\".includes(character) ? character : "_";
      }
    ).join("");
    return sanitized || "missing";
  };
  const remote = Object.keys(remoteTypes)
    .sort()
    .map(name => `${clean(name)}:${remoteTypes[name]}`)
    .join(",");
  const filePickerPortalPref = Services.prefs.getIntPref(
    "widget.use-xdg-desktop-portal.file-picker", -1
  );
  const gtkUsePortal = Services.env.get("GTK_USE_PORTAL");
  done([
    "TD-FIREFOX-SUPPORT-V1",
    `protocol=${clean(graphics.windowProtocol)}`,
    `compositor=${clean(graphics.windowLayerManagerType)}`,
    `adapter=${clean(graphics.adapterDescription)}`,
    `seccomp_bpf=${clean(sandbox.hasSeccompBPF)}`,
    `seccomp_tsync=${clean(sandbox.hasSeccompTSync)}`,
    `privileged_userns=${clean(sandbox.hasPrivilegedUserNamespaces)}`,
    `userns=${clean(sandbox.hasUserNamespaces)}`,
    `content_sandbox=${clean(sandbox.canSandboxContent)}`,
    `media_sandbox=${clean(sandbox.canSandboxMedia)}`,
    `configured=${clean(sandbox.contentSandboxLevel)}`,
    `effective=${clean(sandbox.effectiveContentSandboxLevel)}`,
    `file_picker_portal_pref=${clean(filePickerPortalPref)}`,
    `gtk_use_portal=${clean(gtkUsePortal)}`,
    `roles=${roleReports.join(",")}`,
    `media=${Array.from(mediaUtilityPids).sort((a, b) => a - b).join(".") || "none"}`,
    `remote=${remote || "missing"}`,
  ].join("|"));
})().catch(error => done(
  "TD-FIREFOX-SUPPORT-ERROR:" +
  String(error).replace(/[|,"\\\r\n]/g, "_").slice(0, 256)
));
"#;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RoleReport {
    name: String,
    namespace_pids: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SupportReport {
    protocol: String,
    compositor: String,
    adapter: String,
    seccomp_bpf: bool,
    seccomp_tsync: bool,
    privileged_userns: bool,
    userns: bool,
    content_sandbox: bool,
    media_sandbox: bool,
    configured: u32,
    effective: u32,
    file_picker_portal_pref: u32,
    gtk_use_portal: String,
    roles: Vec<RoleReport>,
    media_utility_pids: Vec<u32>,
    remote: String,
}

pub(crate) fn probe_support() -> io::Result<SupportReport> {
    let address = SocketAddr::from(([127, 0, 0, 1], MARIONETTE_PORT));
    let deadline = Instant::now()
        .checked_add(PROBE_DEADLINE)
        .ok_or_else(|| io::Error::other("Firefox probe deadline overflow"))?;
    let stream = TcpStream::connect_timeout(&address, remaining(deadline)?)
        .map_err(|error| contextual("connect to Firefox Marionette on loopback", error))?;
    let mut stream = DeadlineStream { stream, deadline };

    probe_stream(&mut stream)
}

pub(crate) fn probe_network() -> io::Result<&'static str> {
    let address = SocketAddr::from(([127, 0, 0, 1], MARIONETTE_PORT));
    let deadline = Instant::now()
        .checked_add(NETWORK_PROBE_DEADLINE)
        .ok_or_else(|| io::Error::other("Firefox network probe deadline overflow"))?;
    let stream = TcpStream::connect_timeout(&address, remaining(deadline)?)
        .map_err(|error| contextual("connect to Firefox Marionette on loopback", error))?;
    let mut stream = DeadlineStream { stream, deadline };
    probe_network_stream(&mut stream)?;
    Ok(FIREFOX_NETWORK_RUNTIME_MARKER)
}

pub(crate) fn probe_input<W: Write>(
    stage: InputStage,
    progress: &mut W,
) -> io::Result<&'static str> {
    let address = SocketAddr::from(([127, 0, 0, 1], MARIONETTE_PORT));
    let timeout = if stage == InputStage::Download {
        DOWNLOAD_PROBE_DEADLINE
    } else {
        PROBE_DEADLINE
    };
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::other("Firefox input probe deadline overflow"))?;
    let stream = TcpStream::connect_timeout(&address, remaining(deadline)?)
        .map_err(|error| contextual("connect to Firefox Marionette on loopback", error))?;
    let mut stream = DeadlineStream { stream, deadline };
    probe_input_stream_with_progress(&mut stream, stage, progress)?;
    Ok(stage.marker())
}

pub(crate) fn probe_download() -> io::Result<String> {
    validate_download(
        Path::new(DOWNLOAD_DIRECTORY),
        DOWNLOAD_BYTES,
        DOWNLOAD_UID,
        DOWNLOAD_GID,
    )?;
    Ok(format!(
        "TD-FIREFOX-DOWNLOAD-OK bytes={}",
        DOWNLOAD_BYTES.len()
    ))
}

fn validate_download(directory: &Path, expected: &[u8], uid: u32, gid: u32) -> io::Result<()> {
    let directory_before = fs::symlink_metadata(directory)
        .map_err(|error| contextual("inspect Firefox download directory", error))?;
    if !directory_before.file_type().is_dir()
        || directory_before.uid() != uid
        || directory_before.gid() != gid
        || directory_before.mode() & 0o7777 != 0o700
    {
        return Err(io::Error::other(
            "Firefox download directory has the wrong type, owner, or mode",
        ));
    }

    let path = directory.join(DOWNLOAD_NAME);
    let path_before = fs::symlink_metadata(&path)
        .map_err(|error| contextual("inspect Firefox download path", error))?;
    let mut file = File::open(&path)
        .map_err(|error| contextual("open Firefox download path", error))?;
    let opened = file
        .metadata()
        .map_err(|error| contextual("inspect open Firefox download", error))?;
    require_same_file(&path_before, &opened)?;
    let mode = opened.mode();
    if !opened.file_type().is_file()
        || opened.uid() != uid
        || opened.gid() != gid
        || opened.nlink() != 1
        || mode & 0o7000 != 0
        || mode & 0o700 != 0o600
        || mode & 0o033 != 0
        || opened.len() != expected.len() as u64
    {
        return Err(io::Error::other(
            "Firefox download has the wrong type, owner, mode, links, or size",
        ));
    }

    let mut bytes = Vec::with_capacity(expected.len().saturating_add(1));
    Read::take(&mut file, expected.len().saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| contextual("read Firefox download", error))?;
    if bytes != expected {
        return Err(io::Error::other(
            "Firefox download did not contain the authenticated fixture bytes",
        ));
    }

    let opened_after = file
        .metadata()
        .map_err(|error| contextual("reinspect open Firefox download", error))?;
    let path_after = fs::symlink_metadata(&path)
        .map_err(|error| contextual("reinspect Firefox download path", error))?;
    require_same_file(&opened, &opened_after)?;
    require_same_file(&opened, &path_after)?;

    let mut matching = 0_usize;
    let prefix = b"td-firefox-download";
    for (index, entry) in fs::read_dir(directory)
        .map_err(|error| contextual("enumerate Firefox download directory", error))?
        .enumerate()
    {
        if index >= MAX_DOWNLOAD_DIRECTORY_ENTRIES {
            return Err(io::Error::other(
                "Firefox download directory exceeded its 64-entry proof bound",
            ));
        }
        let entry = entry.map_err(|error| contextual("read Firefox download entry", error))?;
        if entry.file_name().as_bytes().starts_with(prefix) {
            matching = matching.saturating_add(1);
        }
    }
    if matching != 1 {
        return Err(io::Error::other(
            "Firefox download proof found a partial or duplicate target",
        ));
    }

    let directory_after = fs::symlink_metadata(directory)
        .map_err(|error| contextual("reinspect Firefox download directory", error))?;
    require_same_file(&directory_before, &directory_after)
}

fn require_same_file(before: &fs::Metadata, after: &fs::Metadata) -> io::Result<()> {
    if before.dev() == after.dev() && before.ino() == after.ino() {
        return Ok(());
    }
    Err(io::Error::other(
        "Firefox download identity changed during validation",
    ))
}

struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(remaining(self.deadline)?))
            .map_err(|error| contextual("set Firefox probe read deadline", error))?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream
            .set_write_timeout(Some(remaining(self.deadline)?))
            .map_err(|error| contextual("set Firefox probe write deadline", error))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream
            .set_write_timeout(Some(remaining(self.deadline)?))
            .map_err(|error| contextual("set Firefox probe flush deadline", error))?;
        self.stream.flush()
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Firefox probe exceeded its wall-clock deadline",
            )
        })
}

fn probe_stream<S: Read + Write>(stream: &mut S) -> io::Result<SupportReport> {
    start_session(stream)?;

    let result = run_session_probe(stream);
    let cleanup = delete_session(stream);
    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(probe), Err(cleanup)) => Err(io::Error::other(format!(
            "Firefox support probe failed: {probe}; session cleanup also failed: {cleanup}"
        ))),
    }
}

fn probe_network_stream<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    start_session(stream)?;
    let result = run_network_probe(stream);
    let cleanup = delete_session_with_id(stream, 5);
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(probe), Err(cleanup)) => Err(io::Error::other(format!(
            "Firefox network probe failed: {probe}; session cleanup also failed: {cleanup}"
        ))),
    }
}

#[cfg(test)]
fn probe_input_stream<S: Read + Write>(stream: &mut S, stage: InputStage) -> io::Result<()> {
    probe_input_stream_with_progress(stream, stage, &mut io::sink())
}

fn probe_input_stream_with_progress<S: Read + Write, W: Write>(
    stream: &mut S,
    stage: InputStage,
    progress: &mut W,
) -> io::Result<()> {
    start_session(stream)?;
    let result = run_input_stage(stream, stage, progress);
    let cleanup = delete_input_session(stream);
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(probe), Err(cleanup)) => Err(io::Error::other(format!(
            "Firefox input probe failed: {probe}; session cleanup also failed: {cleanup}"
        ))),
    }
}

fn start_session<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    let hello = read_frame(stream)
        .map_err(|error| contextual("read Firefox Marionette greeting", error))?;
    require_exact("Marionette greeting", &hello, HELLO)?;
    write_frame(stream, NEW_SESSION)
        .map_err(|error| contextual("write Firefox new-session command", error))?;
    let response = read_frame(stream)
        .map_err(|error| contextual("read Firefox new-session response", error))?;
    if !response.starts_with(NEW_SESSION_PREFIX) || !response.ends_with("}}]") {
        return Err(unexpected("new-session response", &response));
    }
    Ok(())
}

fn run_input_stage<S: Read + Write, W: Write>(
    stream: &mut S,
    stage: InputStage,
    progress: &mut W,
) -> io::Result<()> {
    match stage {
        InputStage::Arm => {
            set_context(stream, 2, "content")?;
            require_script_value(stream, 3, CONTENT_ARM_SCRIPT, INPUT_CONTENT_ARMED)?;
            set_context(stream, 4, "chrome")?;
            require_script_value(stream, 5, CHROME_ARM_SCRIPT, INPUT_CHROME_ARMED)
        }
        InputStage::Menu => {
            set_context(stream, 2, "content")?;
            require_script_value(stream, 3, CONTENT_MENU_SCRIPT, INPUT_CONTENT_OK)?;
            set_context(stream, 4, "chrome")?;
            require_script_value(stream, 5, CHROME_MENU_SCRIPT, INPUT_MENU_OK)
        }
        InputStage::Final => {
            set_context(stream, 2, "chrome")?;
            require_script_value(stream, 3, CHROME_FINAL_SCRIPT, INPUT_FINAL_OK)
        }
        InputStage::ClipboardRefocusArm => {
            set_context(stream, 2, "content")?;
            require_script_value(
                stream,
                3,
                CONTENT_CLIPBOARD_REFOCUS_ARM_SCRIPT,
                INPUT_CLIPBOARD_REFOCUS_ARMED,
            )
        }
        InputStage::ClipboardRefocus => {
            set_context(stream, 2, "content")?;
            require_script_value(
                stream,
                3,
                CONTENT_CLIPBOARD_REFOCUS_SCRIPT,
                INPUT_CLIPBOARD_WINDOW_ARMED,
            )
        }
        InputStage::Clipboard => {
            set_context(stream, 2, "chrome")?;
            require_script_value(
                stream,
                3,
                CHROME_CLIPBOARD_ARM_SCRIPT,
                INPUT_CLIPBOARD_ARMED,
            )?;
            writeln!(progress, "{INPUT_CLIPBOARD_PUBLIC_ARMED}")?;
            progress.flush()?;
            let value = script_value(stream, 4, CHROME_CLIPBOARD_SCRIPT)?;
            if value == INPUT_CLIPBOARD_OK {
                return Ok(());
            }
            if value != INPUT_CLIPBOARD_RETRY {
                return Err(unexpected("input script value", &value));
            }
            writeln!(progress, "{INPUT_CLIPBOARD_PUBLIC_RETRY}")?;
            progress.flush()?;
            require_script_value(stream, 5, CHROME_CLIPBOARD_SCRIPT, INPUT_CLIPBOARD_OK)
        }
        InputStage::Download => {
            set_context(stream, 2, "content")?;
            require_script_value(stream, 3, CONTENT_DOWNLOAD_ARM_SCRIPT, INPUT_DOWNLOAD_ARMED)?;
            writeln!(progress, "{INPUT_DOWNLOAD_PUBLIC_ARMED}")?;
            progress.flush()?;
            require_script_value(stream, 4, CONTENT_DOWNLOAD_SCRIPT, INPUT_DOWNLOAD_CLICKED)
        }
        InputStage::FileChooser => {
            set_context(stream, 2, "content")?;
            require_script_value(
                stream,
                3,
                CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT,
                INPUT_FILE_CHOOSER_REFOCUS_ARMED,
            )?;
            writeln!(progress, "{INPUT_FILE_CHOOSER_PUBLIC_REFOCUS_ARMED}")?;
            progress.flush()?;
            require_script_value(
                stream,
                4,
                CONTENT_FILE_CHOOSER_ARM_SCRIPT,
                INPUT_FILE_CHOOSER_ARMED,
            )
        }
        InputStage::FileChooserFocus => {
            set_context(stream, 2, "content")?;
            require_script_value(
                stream,
                3,
                CONTENT_FILE_CHOOSER_FOCUS_SCRIPT,
                INPUT_FILE_CHOOSER_FOCUSED,
            )
        }
        InputStage::FileChooserResult => {
            set_context(stream, 2, "content")?;
            require_script_value(
                stream,
                3,
                CONTENT_FILE_CHOOSER_SCRIPT,
                INPUT_FILE_CHOOSER_OK,
            )
        }
    }
}

fn set_context<S: Read + Write>(stream: &mut S, id: u8, value: &str) -> io::Result<()> {
    let command = format!(r#"[0,{id},"Marionette:SetContext",{{"value":"{value}"}}]"#);
    write_frame(stream, &command)
        .map_err(|error| contextual("write Firefox set-context command", error))?;
    let response = read_frame(stream)
        .map_err(|error| contextual("read Firefox set-context response", error))?;
    require_exact(
        "set-context response",
        &response,
        &format!(r#"[1,{id},null,{{"value":null}}]"#),
    )
}

fn run_network_probe<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    set_context(stream, 2, "content")?;
    navigate(stream, 3, FIREFOX_NETWORK_TEST_URL)?;
    let script = network_document_script();
    require_script_value(
        stream,
        4,
        &script,
        FIREFOX_NETWORK_CONTENT_OK,
    )
}

fn navigate<S: Read + Write>(stream: &mut S, id: u8, url: &str) -> io::Result<()> {
    let encoded = json_string(url);
    let command = format!(r#"[0,{id},"WebDriver:Navigate",{{"url":{encoded}}}]"#);
    if command.len() > MAX_COMMAND_BYTES {
        return Err(io::Error::other(
            "Firefox navigation command exceeded its 64 KiB bound",
        ));
    }
    write_frame(stream, &command)
        .map_err(|error| contextual("write Firefox navigation command", error))?;
    let response = read_frame(stream)
        .map_err(|error| contextual("read Firefox navigation response", error))?;
    require_exact(
        "navigation response",
        &response,
        &format!(r#"[1,{id},null,{{"value":null}}]"#),
    )
}

fn require_script_value<S: Read + Write>(
    stream: &mut S,
    id: u8,
    script: &str,
    expected: &str,
) -> io::Result<()> {
    let value = script_value(stream, id, script)?;
    require_exact("input script value", &value, expected)
}

fn script_value<S: Read + Write>(stream: &mut S, id: u8, script: &str) -> io::Result<String> {
    let command = execute_command_with_id(id, script)?;
    write_frame(stream, &command)
        .map_err(|error| contextual("write Firefox input script", error))?;
    let response = read_frame(stream)
        .map_err(|error| contextual("read Firefox input script response", error))?;
    let prefix = format!(r#"[1,{id},null,{{"value":""#);
    let value = response
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(EXECUTE_RESPONSE_SUFFIX))
        .ok_or_else(|| unexpected("execute-script response", &response))?;
    Ok(value.to_string())
}

fn delete_input_session<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    delete_session_with_id(stream, 6)
}

fn run_session_probe<S: Read + Write>(stream: &mut S) -> io::Result<SupportReport> {
    set_context(stream, 2, "chrome")?;

    let command = execute_command(REPORT_SCRIPT)?;
    write_frame(stream, &command)
        .map_err(|error| contextual("write Firefox support script", error))?;
    let response = read_frame(stream)
        .map_err(|error| contextual("read Firefox support script response", error))?;
    let value = response
        .strip_prefix(EXECUTE_RESPONSE_PREFIX)
        .and_then(|rest| rest.strip_suffix(EXECUTE_RESPONSE_SUFFIX))
        .ok_or_else(|| unexpected("execute-script response", &response))?;
    if value.contains(['"', '\\'])
        || !value
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
    {
        return Err(io::Error::other(format!(
            "Firefox support report used an escaped or non-ASCII value: {}",
            printable(value)
        )));
    }
    parse_report(value)
}

fn delete_session<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    delete_session_with_id(stream, 4)
}

fn delete_session_with_id<S: Read + Write>(stream: &mut S, id: u8) -> io::Result<()> {
    let command = format!(r#"[0,{id},"WebDriver:DeleteSession",{{}}]"#);
    write_frame(stream, &command)
        .map_err(|error| contextual("write Firefox delete-session command", error))?;
    let response = read_frame(stream)
        .map_err(|error| contextual("read Firefox delete-session response", error))?;
    require_exact(
        "delete-session response",
        &response,
        &format!(r#"[1,{id},null,{{"value":null}}]"#),
    )
}

fn execute_command(script: &str) -> io::Result<String> {
    execute_command_with_id(3, script)
}

fn execute_command_with_id(id: u8, script: &str) -> io::Result<String> {
    let escaped = json_string(script);
    let command =
        format!("[0,{id},\"WebDriver:ExecuteAsyncScript\",{{\"script\":{escaped},\"args\":[]}}]");
    if command.len() > MAX_COMMAND_BYTES {
        return Err(io::Error::other(
            "Firefox Marionette command exceeded its 64 KiB bound",
        ));
    }
    Ok(command)
}

fn json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len().saturating_add(2));
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            value if (value as u32) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", value as u32));
            }
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

fn write_frame<W: Write>(writer: &mut W, body: &str) -> io::Result<()> {
    if body.is_empty() || body.len() > MAX_COMMAND_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Marionette command length is outside its bound",
        ));
    }
    write!(writer, "{}:", body.len())?;
    writer.write_all(body.as_bytes())
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut length = 0_usize;
    let mut digits = 0_usize;
    let mut first = None;
    loop {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte)?;
        match byte.first().copied() {
            Some(b':') if digits > 0 => break,
            Some(value @ b'0'..=b'9') if digits < MAX_HEADER_DIGITS => {
                if first.is_none() {
                    first = Some(value);
                }
                length = length
                    .checked_mul(10)
                    .and_then(|current| current.checked_add(usize::from(value - b'0')))
                    .ok_or_else(|| io::Error::other("Marionette frame length overflowed"))?;
                digits = digits.saturating_add(1);
            }
            _ => {
                return Err(io::Error::other(
                    "Marionette frame header is not canonical decimal",
                ));
            }
        }
    }
    if length == 0 || length > MAX_FRAME_BYTES || (digits > 1 && first == Some(b'0')) {
        return Err(io::Error::other(
            "Marionette frame length is empty, over limit, or noncanonical",
        ));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    String::from_utf8(body)
        .map_err(|error| io::Error::other(format!("Marionette frame is not UTF-8: {error}")))
}

fn parse_report(value: &str) -> io::Result<SupportReport> {
    let mut fields = value.split('|');
    if fields.next() != Some(REPORT_PREFIX) {
        return Err(io::Error::other(format!(
            "Firefox returned an unexpected support report: {}",
            printable(value)
        )));
    }
    let protocol = take_field(&mut fields, "protocol")?;
    let compositor = take_field(&mut fields, "compositor")?;
    let adapter = take_field(&mut fields, "adapter")?;
    let seccomp_bpf = parse_bool("seccomp_bpf", &take_field(&mut fields, "seccomp_bpf")?)?;
    let seccomp_tsync = parse_bool("seccomp_tsync", &take_field(&mut fields, "seccomp_tsync")?)?;
    let privileged_userns = parse_bool(
        "privileged_userns",
        &take_field(&mut fields, "privileged_userns")?,
    )?;
    let userns = parse_bool("userns", &take_field(&mut fields, "userns")?)?;
    let content_sandbox = parse_bool(
        "content_sandbox",
        &take_field(&mut fields, "content_sandbox")?,
    )?;
    let media_sandbox = parse_bool("media_sandbox", &take_field(&mut fields, "media_sandbox")?)?;
    let configured = parse_u32("configured", &take_field(&mut fields, "configured")?)?;
    let effective = parse_u32("effective", &take_field(&mut fields, "effective")?)?;
    let file_picker_portal_pref = parse_u32(
        "file_picker_portal_pref",
        &take_field(&mut fields, "file_picker_portal_pref")?,
    )?;
    let gtk_use_portal = take_field(&mut fields, "gtk_use_portal")?;
    let roles = parse_roles(&take_field(&mut fields, "roles")?)?;
    let media_utility_pids = parse_pid_list(&take_field(&mut fields, "media")?)?;
    let remote = take_field(&mut fields, "remote")?;
    if fields.next().is_some() {
        return Err(io::Error::other(
            "Firefox support report carried trailing fields",
        ));
    }
    Ok(SupportReport {
        protocol,
        compositor,
        adapter,
        seccomp_bpf,
        seccomp_tsync,
        privileged_userns,
        userns,
        content_sandbox,
        media_sandbox,
        configured,
        effective,
        file_picker_portal_pref,
        gtk_use_portal,
        roles,
        media_utility_pids,
        remote,
    })
}

fn take_field<'a, I>(fields: &mut I, name: &str) -> io::Result<String>
where
    I: Iterator<Item = &'a str>,
{
    let field = fields
        .next()
        .ok_or_else(|| io::Error::other(format!("Firefox support report omitted {name}")))?;
    let expected = format!("{name}=");
    let value = field.strip_prefix(&expected).ok_or_else(|| {
        io::Error::other(format!(
            "Firefox support report expected {name}, got {}",
            printable(field)
        ))
    })?;
    if value.is_empty() || value.len() > 4096 {
        return Err(io::Error::other(format!(
            "Firefox support field {name} is empty or over limit"
        )));
    }
    Ok(value.to_string())
}

fn parse_bool(name: &str, value: &str) -> io::Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(io::Error::other(format!(
            "Firefox support field {name} is not boolean: {}",
            printable(value)
        ))),
    }
}

fn parse_u32(name: &str, value: &str) -> io::Result<u32> {
    value.parse::<u32>().map_err(|error| {
        io::Error::other(format!(
            "Firefox support field {name} is not a bounded integer: {error}"
        ))
    })
}

fn parse_roles(value: &str) -> io::Result<Vec<RoleReport>> {
    let mut reports = Vec::new();
    let mut total_pids = 0_usize;
    for row in value.split(',') {
        let mut fields = row.splitn(2, ':');
        let name = fields
            .next()
            .filter(|name| matches!(*name, "content" | "gpu" | "socket" | "rdd" | "utility"))
            .ok_or_else(|| io::Error::other("Firefox support report has an unknown role"))?;
        if reports
            .iter()
            .any(|report: &RoleReport| report.name == name)
        {
            return Err(io::Error::other(
                "Firefox support report repeats a process role",
            ));
        }
        let encoded = fields
            .next()
            .ok_or_else(|| io::Error::other(format!("Firefox role {name} omitted its PIDs")))?;
        let mut namespace_pids = Vec::new();
        if !encoded.is_empty() {
            for field in encoded.split('.') {
                let pid = parse_u32(&format!("role {name} PID"), field)?;
                if pid == 0
                    || namespace_pids.contains(&pid)
                    || reports
                        .iter()
                        .any(|report: &RoleReport| report.namespace_pids.contains(&pid))
                {
                    return Err(io::Error::other(format!(
                        "Firefox role {name} has a zero or repeated PID"
                    )));
                }
                total_pids = total_pids.saturating_add(1);
                if total_pids > 256 {
                    return Err(io::Error::other(
                        "Firefox support report exceeded 256 child PIDs",
                    ));
                }
                namespace_pids.push(pid);
            }
        }
        reports.push(RoleReport {
            name: name.to_string(),
            namespace_pids,
        });
    }
    if reports.len() != 5 {
        return Err(io::Error::other(
            "Firefox support report did not name all five process roles",
        ));
    }
    Ok(reports)
}

fn parse_pid_list(value: &str) -> io::Result<Vec<u32>> {
    if value == "none" {
        return Ok(Vec::new());
    }
    let mut pids = Vec::new();
    for field in value.split('.') {
        let pid = parse_u32("media utility PID", field)?;
        if pid == 0 || pids.contains(&pid) || pids.len() >= 256 {
            return Err(io::Error::other(
                "Firefox media utility PID set is zero, repeated, or over limit",
            ));
        }
        pids.push(pid);
    }
    Ok(pids)
}

pub(crate) fn namespace_pids(report: &SupportReport) -> Vec<u32> {
    report
        .roles
        .iter()
        .flat_map(|role| role.namespace_pids.iter().copied())
        .collect()
}

pub(crate) fn validate_and_render(
    report: &SupportReport,
    sandboxes: &[ProcessSandbox],
) -> io::Result<String> {
    validate_report(report, sandboxes)?;
    Ok(format!(
        "TD-FIREFOX-SUPPORT-OK protocol={} compositor={} adapter={} roles={} media={} remote={}",
        report.protocol,
        report.compositor,
        report.adapter,
        render_roles(&report.roles, sandboxes),
        render_pids(&report.media_utility_pids),
        report.remote
    ))
}

fn validate_report(report: &SupportReport, sandboxes: &[ProcessSandbox]) -> io::Result<()> {
    if report.protocol != "wayland" {
        return Err(report_error(
            report,
            sandboxes,
            "window protocol is not Wayland",
        ));
    }
    if report.compositor != "WebRender (Software)" {
        return Err(report_error(
            report,
            sandboxes,
            "compositing is not Software WebRender",
        ));
    }
    if !report.seccomp_bpf
        || !report.seccomp_tsync
        || report.privileged_userns
        || report.userns
        || !report.content_sandbox
        || !report.media_sandbox
        || report.configured != 6
        || report.effective != 6
    {
        return Err(report_error(
            report,
            sandboxes,
            "about:support sandbox facts differ from the pinned fallback policy",
        ));
    }
    if report.file_picker_portal_pref != 2 || report.gtk_use_portal != "1" {
        return Err(report_error(
            report,
            sandboxes,
            "Firefox's file-picker portal policy is not the pinned automatic policy",
        ));
    }
    for required in ["content", "socket"] {
        require_role(report, sandboxes, required)?;
    }
    let rdd = role(report, "rdd");
    let utility = role(report, "utility");
    let media_utility = !report.media_utility_pids.is_empty();
    if media_utility
        && !utility.is_some_and(|role| {
            report
                .media_utility_pids
                .iter()
                .all(|pid| role.namespace_pids.contains(pid))
        })
    {
        return Err(report_error(
            report,
            sandboxes,
            "media utility PID is not a reported utility process",
        ));
    }
    if !rdd.is_some_and(|role| role_is_sandboxed(role, sandboxes)) && !media_utility {
        return Err(report_error(
            report,
            sandboxes,
            "neither Firefox media role carried a nested seccomp filter",
        ));
    }
    for observed in &report.roles {
        if !observed.namespace_pids.is_empty() && !role_is_sandboxed(observed, sandboxes) {
            return Err(report_error(
                report,
                sandboxes,
                &format!(
                    "Firefox role {} contains a process without its nested seccomp filter",
                    observed.name
                ),
            ));
        }
    }
    Ok(())
}

fn require_role(
    report: &SupportReport,
    sandboxes: &[ProcessSandbox],
    name: &str,
) -> io::Result<()> {
    if role(report, name).is_some_and(|role| role_is_sandboxed(role, sandboxes)) {
        return Ok(());
    }
    Err(report_error(
        report,
        sandboxes,
        &format!("Firefox role {name} is absent or lacks its nested seccomp filter"),
    ))
}

fn role<'a>(report: &'a SupportReport, name: &str) -> Option<&'a RoleReport> {
    report.roles.iter().find(|role| role.name == name)
}

fn role_is_sandboxed(role: &RoleReport, sandboxes: &[ProcessSandbox]) -> bool {
    !role.namespace_pids.is_empty()
        && role.namespace_pids.iter().all(|pid| {
            sandboxes
                .iter()
                .any(|sandbox| sandbox.namespace_pid == *pid && sandbox_is_nested(sandbox))
        })
}

fn sandbox_is_nested(sandbox: &ProcessSandbox) -> bool {
    sandbox.no_new_privileges == 1 && sandbox.seccomp == 2 && sandbox.filters >= 2
}

fn report_error(report: &SupportReport, sandboxes: &[ProcessSandbox], message: &str) -> io::Error {
    io::Error::other(format!(
        "{message}: protocol={} compositor={} adapter={} sandbox={}/{}/{}/{}/{}/{}/{}:{} file-picker={}/{} roles={} media={} remote={}",
        report.protocol,
        report.compositor,
        report.adapter,
        report.seccomp_bpf,
        report.seccomp_tsync,
        report.privileged_userns,
        report.userns,
        report.content_sandbox,
        report.media_sandbox,
        report.configured,
        report.effective,
        report.file_picker_portal_pref,
        report.gtk_use_portal,
        render_roles(&report.roles, sandboxes),
        render_pids(&report.media_utility_pids),
        report.remote
    ))
}

fn render_pids(pids: &[u32]) -> String {
    if pids.is_empty() {
        return "none".to_string();
    }
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

fn render_roles(roles: &[RoleReport], sandboxes: &[ProcessSandbox]) -> String {
    roles
        .iter()
        .map(|role| {
            let statuses = role
                .namespace_pids
                .iter()
                .filter_map(|pid| sandboxes.iter().find(|status| status.namespace_pid == *pid))
                .collect::<Vec<_>>();
            let no_new_privileges = statuses
                .iter()
                .map(|status| status.no_new_privileges)
                .min()
                .unwrap_or(0);
            let seccomp = statuses
                .iter()
                .map(|status| status.seccomp)
                .min()
                .unwrap_or(0);
            let filters = statuses
                .iter()
                .map(|status| status.filters)
                .min()
                .unwrap_or(0);
            format!(
                "{}:{}/{}/{}/{}",
                role.name,
                role.namespace_pids.len(),
                no_new_privileges,
                seccomp,
                filters
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn require_exact(name: &str, actual: &str, expected: &str) -> io::Result<()> {
    if actual == expected {
        return Ok(());
    }
    Err(unexpected(name, actual))
}

fn unexpected(name: &str, value: &str) -> io::Error {
    io::Error::other(format!(
        "Firefox returned an unexpected {name}: {}",
        printable(value)
    ))
}

fn printable(value: &str) -> String {
    value
        .chars()
        .take(512)
        .map(|character| {
            if character == ' ' || character.is_ascii_graphic() {
                character
            } else {
                '?'
            }
        })
        .collect()
}

fn contextual(context: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn download_test_directory() -> std::path::PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "td-firefox-download-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    struct ScriptedIo {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for ScriptedIo {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for ScriptedIo {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const GOOD_REPORT: &str = "TD-FIREFOX-SUPPORT-V1|protocol=wayland|compositor=WebRender (Software)|adapter=llvmpipe|seccomp_bpf=true|seccomp_tsync=true|privileged_userns=false|userns=false|content_sandbox=true|media_sandbox=true|configured=6|effective=6|file_picker_portal_pref=2|gtk_use_portal=1|roles=content:11.12,gpu:,socket:13,rdd:14,utility:15|media=none|remote=rdd:1,socket:1,utility_jSOracle:1,web:2";

    fn good_sandboxes() -> Vec<ProcessSandbox> {
        [11, 12, 13, 14, 15]
            .into_iter()
            .map(|namespace_pid| ProcessSandbox {
                namespace_pid,
                no_new_privileges: 1,
                seccomp: 2,
                filters: 2,
            })
            .collect()
    }

    #[test]
    fn frame_codec_is_canonical_and_bounded() {
        let mut encoded = Vec::new();
        write_frame(&mut encoded, "[1,2,null,{}]").unwrap();
        assert_eq!(encoded, b"13:[1,2,null,{}]");
        assert_eq!(
            read_frame(&mut Cursor::new(encoded)).unwrap(),
            "[1,2,null,{}]"
        );

        for malformed in [
            b"0:".as_slice(),
            b"02:{}".as_slice(),
            b"x:{}".as_slice(),
            b"12345678:".as_slice(),
            b"1048577:".as_slice(),
            b"4:{}".as_slice(),
        ] {
            assert!(read_frame(&mut Cursor::new(malformed)).is_err());
        }
    }

    #[test]
    fn production_probe_has_one_end_to_end_deadline() {
        assert_eq!(PROBE_DEADLINE, Duration::from_secs(60));
        assert_eq!(NETWORK_PROBE_DEADLINE, Duration::from_secs(60));
        assert_eq!(DOWNLOAD_PROBE_DEADLINE, Duration::from_secs(40));
        assert_eq!(
            remaining(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn report_script_is_one_bounded_privileged_snapshot() {
        let command = execute_command(REPORT_SCRIPT).unwrap();
        assert!(command.starts_with(r#"[0,3,"WebDriver:ExecuteAsyncScript",{"script":"#));
        assert!(command.contains("Troubleshoot.snapshot()"));
        assert!(command.contains("ChromeUtils.requestProcInfo()"));
        assert!(command.contains("Array.from(pids)"));
        assert!(command.contains("child.utilityActors || []"));
        assert!(command.contains("mediaUtilityActors.has"));
        assert!(command.contains("nonMediaUtilityActors.has"));
        assert!(command.contains("audioDecoder_Generic"));
        assert!(command.contains("contentTypes.has"));
        assert!(command.contains("unknown process type"));
        assert!(!REPORT_SCRIPT.contains("type.startsWith"));
        for process_type in [
            "web",
            "webIsolated",
            "file",
            "extension",
            "privilegedabout",
            "privilegedmozilla",
            "withCoopCoep",
            "webServiceWorker",
            "preallocated",
            "inference",
        ] {
            assert!(REPORT_SCRIPT.contains(&format!("\"{process_type}\"")));
        }
        for actor in [
            "jSOracle",
            "windowsUtils",
            "windowsFileDialog",
            "pkcs11Module",
        ] {
            assert!(REPORT_SCRIPT.contains(&format!("\"{actor}\"")));
        }
        assert_eq!(REPORT_SCRIPT.matches("type === \"utility\"").count(), 1);
        assert!(!REPORT_SCRIPT.contains("roles.has(type)"));
        assert!(command.contains("windowProtocol"));
        assert!(command.contains("windowLayerManagerType"));
        assert!(command.contains("code <= 0x7e"));
        assert!(REPORT_SCRIPT.contains("sanitized || \"missing\""));
        assert!(REPORT_SCRIPT.contains("remote || \"missing\""));
        assert!(command.len() < MAX_COMMAND_BYTES);
    }

    #[test]
    fn complete_protocol_transcript_is_exact_and_cleans_up() {
        let responses = [
            HELLO.to_string(),
            r#"[1,1,null,{"sessionId":"td","capabilities":{}}]"#.to_string(),
            r#"[1,2,null,{"value":null}]"#.to_string(),
            format!("{EXECUTE_RESPONSE_PREFIX}{GOOD_REPORT}{EXECUTE_RESPONSE_SUFFIX}"),
            r#"[1,4,null,{"value":null}]"#.to_string(),
        ];
        let mut input = Vec::new();
        for response in responses {
            write_frame(&mut input, &response).unwrap();
        }
        let mut io = ScriptedIo {
            input: Cursor::new(input),
            output: Vec::new(),
        };

        let report = probe_stream(&mut io).unwrap();
        assert!(validate_and_render(&report, &good_sandboxes())
            .unwrap()
            .starts_with("TD-FIREFOX-SUPPORT-OK protocol=wayland"));

        let mut commands = Cursor::new(io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"chrome"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command(REPORT_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,4,"WebDriver:DeleteSession",{}]"#
        );
        assert!(read_frame(&mut commands).is_err());
    }

    fn network_transcript(value: &str) -> ScriptedIo {
        let responses = [
            HELLO.to_string(),
            r#"[1,1,null,{"sessionId":"td","capabilities":{}}]"#.to_string(),
            r#"[1,2,null,{"value":null}]"#.to_string(),
            r#"[1,3,null,{"value":null}]"#.to_string(),
            format!(r#"[1,4,null,{{"value":"{value}"}}]"#),
            r#"[1,5,null,{"value":null}]"#.to_string(),
        ];
        let mut input = Vec::new();
        for response in responses {
            write_frame(&mut input, &response).unwrap();
        }
        ScriptedIo {
            input: Cursor::new(input),
            output: Vec::new(),
        }
    }

    #[test]
    fn network_protocol_navigates_and_validates_the_public_document() {
        assert_eq!(FIREFOX_NETWORK_TEST_URL, "https://git.kernel.org/");
        assert_eq!(
            FIREFOX_NETWORK_RUNTIME_MARKER,
            "TD-FIREFOX-NETWORK-HTTPS-OK"
        );
        let script = network_document_script();
        assert!(script.contains("document.readyState"));
        assert!(script.contains("url.origin !== expected.origin"));
        assert!(script.contains("documentUrl.origin !== expected.origin"));
        assert!(script.contains("Number(navigation.responseStatus) !== 200"));
        assert!(script.contains(".slice(0, 4097)"));
        assert_eq!(script.matches(FIREFOX_NETWORK_TEST_URL).count(), 1);
        assert!(!script.contains("url.hostname"));

        let mut io = network_transcript(FIREFOX_NETWORK_CONTENT_OK);
        probe_network_stream(&mut io).unwrap();
        let mut commands = Cursor::new(io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,3,"WebDriver:Navigate",{"url":"https://git.kernel.org/"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(4, &script).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,5,"WebDriver:DeleteSession",{}]"#
        );
        assert!(read_frame(&mut commands).is_err());
    }

    #[test]
    fn network_protocol_rejects_the_wrong_document_and_still_cleans_up() {
        let mut io = network_transcript("TD-FIREFOX-NETWORK-ERROR:wrong-origin");
        assert!(probe_network_stream(&mut io).is_err());
        let mut commands = Cursor::new(io.output);
        let mut last = String::new();
        while let Ok(command) = read_frame(&mut commands) {
            last = command;
        }
        assert_eq!(last, r#"[0,5,"WebDriver:DeleteSession",{}]"#);
    }

    #[test]
    fn network_protocol_rejects_a_navigation_error_and_still_cleans_up() {
        let responses = [
            HELLO,
            r#"[1,1,null,{"sessionId":"td","capabilities":{}}]"#,
            r#"[1,2,null,{"value":null}]"#,
            r#"[1,3,{"error":"unknown error","message":"Reached error page: about:neterror"},null]"#,
            r#"[1,5,null,{"value":null}]"#,
        ];
        let mut input = Vec::new();
        for response in responses {
            write_frame(&mut input, response).unwrap();
        }
        let mut io = ScriptedIo {
            input: Cursor::new(input),
            output: Vec::new(),
        };
        assert!(probe_network_stream(&mut io).is_err());
        let mut commands = Cursor::new(io.output);
        let mut observed = Vec::new();
        while let Ok(command) = read_frame(&mut commands) {
            observed.push(command);
        }
        assert_eq!(
            observed,
            [
                NEW_SESSION,
                r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#,
                r#"[0,3,"WebDriver:Navigate",{"url":"https://git.kernel.org/"}]"#,
                r#"[0,5,"WebDriver:DeleteSession",{}]"#,
            ]
        );
    }

    fn input_transcript(stage: InputStage, values: &[&str]) -> ScriptedIo {
        let mut responses = vec![
            HELLO.to_string(),
            r#"[1,1,null,{"sessionId":"td","capabilities":{}}]"#.to_string(),
        ];
        match stage {
            InputStage::Arm | InputStage::Menu => {
                responses.push(r#"[1,2,null,{"value":null}]"#.to_string());
                responses.push(format!(
                    "[1,3,null,{{\"value\":\"{}\"}}]",
                    values.first().copied().unwrap_or_default()
                ));
                responses.push(r#"[1,4,null,{"value":null}]"#.to_string());
                responses.push(format!(
                    "[1,5,null,{{\"value\":\"{}\"}}]",
                    values.get(1).copied().unwrap_or_default()
                ));
            }
            InputStage::Final
            | InputStage::ClipboardRefocusArm
            | InputStage::ClipboardRefocus => {
                responses.push(r#"[1,2,null,{"value":null}]"#.to_string());
                responses.push(format!(
                    "[1,3,null,{{\"value\":\"{}\"}}]",
                    values.first().copied().unwrap_or_default()
                ));
            }
            InputStage::Clipboard => {
                responses.push(r#"[1,2,null,{"value":null}]"#.to_string());
                for (index, value) in values.iter().enumerate() {
                    let id = if index == 0 { 3 } else { index + 3 };
                    responses.push(format!("[1,{id},null,{{\"value\":\"{value}\"}}]"));
                }
            }
            InputStage::Download
            | InputStage::FileChooser
            | InputStage::FileChooserResult => {
                responses.push(r#"[1,2,null,{"value":null}]"#.to_string());
                for (index, value) in values.iter().enumerate() {
                    let id = index + 3;
                    responses.push(format!("[1,{id},null,{{\"value\":\"{value}\"}}]"));
                }
            }
            InputStage::FileChooserFocus => {
                responses.push(r#"[1,2,null,{"value":null}]"#.to_string());
                responses.push(format!(
                    "[1,3,null,{{\"value\":\"{}\"}}]",
                    values.first().copied().unwrap_or_default()
                ));
            }
        }
        responses.push(r#"[1,6,null,{"value":null}]"#.to_string());
        let mut input = Vec::new();
        for response in responses {
            write_frame(&mut input, &response).unwrap();
        }
        ScriptedIo {
            input: Cursor::new(input),
            output: Vec::new(),
        }
    }

    #[test]
    fn staged_input_protocol_is_exact_and_fail_closed() {
        for (stage, scripts, values) in [
            (
                InputStage::Arm,
                [CONTENT_ARM_SCRIPT, CHROME_ARM_SCRIPT],
                [INPUT_CONTENT_ARMED, INPUT_CHROME_ARMED],
            ),
            (
                InputStage::Menu,
                [CONTENT_MENU_SCRIPT, CHROME_MENU_SCRIPT],
                [INPUT_CONTENT_OK, INPUT_MENU_OK],
            ),
        ] {
            let mut io = input_transcript(stage, &values);
            probe_input_stream(&mut io, stage).unwrap();
            let mut commands = Cursor::new(io.output);
            assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
            assert_eq!(
                read_frame(&mut commands).unwrap(),
                r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#
            );
            assert_eq!(
                read_frame(&mut commands).unwrap(),
                execute_command_with_id(3, scripts[0]).unwrap()
            );
            assert_eq!(
                read_frame(&mut commands).unwrap(),
                r#"[0,4,"Marionette:SetContext",{"value":"chrome"}]"#
            );
            assert_eq!(
                read_frame(&mut commands).unwrap(),
                execute_command_with_id(5, scripts[1]).unwrap()
            );
            assert_eq!(
                read_frame(&mut commands).unwrap(),
                r#"[0,6,"WebDriver:DeleteSession",{}]"#
            );
            assert!(read_frame(&mut commands).is_err());
        }

        let mut final_io = input_transcript(InputStage::Final, &[INPUT_FINAL_OK]);
        probe_input_stream(&mut final_io, InputStage::Final).unwrap();
        let mut commands = Cursor::new(final_io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"chrome"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(3, CHROME_FINAL_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );

        let mut download_io = input_transcript(
            InputStage::Download,
            &[INPUT_DOWNLOAD_ARMED, INPUT_DOWNLOAD_CLICKED],
        );
        let mut download_progress = Vec::new();
        probe_input_stream_with_progress(
            &mut download_io,
            InputStage::Download,
            &mut download_progress,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(download_progress).unwrap(),
            format!("{INPUT_DOWNLOAD_PUBLIC_ARMED}\n")
        );
        let mut commands = Cursor::new(download_io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(3, CONTENT_DOWNLOAD_ARM_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(4, CONTENT_DOWNLOAD_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );

        let mut chooser_io = input_transcript(
            InputStage::FileChooser,
            &[INPUT_FILE_CHOOSER_REFOCUS_ARMED, INPUT_FILE_CHOOSER_ARMED],
        );
        let mut chooser_progress = Vec::new();
        probe_input_stream_with_progress(
            &mut chooser_io,
            InputStage::FileChooser,
            &mut chooser_progress,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(chooser_progress).unwrap(),
            format!("{INPUT_FILE_CHOOSER_PUBLIC_REFOCUS_ARMED}\n")
        );
        assert_eq!(
            InputStage::FileChooser.marker(),
            INPUT_FILE_CHOOSER_PUBLIC_ARMED
        );
        let mut commands = Cursor::new(chooser_io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(3, CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(4, CONTENT_FILE_CHOOSER_ARM_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );

        let mut chooser_focus_io =
            input_transcript(InputStage::FileChooserFocus, &[INPUT_FILE_CHOOSER_FOCUSED]);
        probe_input_stream_with_progress(
            &mut chooser_focus_io,
            InputStage::FileChooserFocus,
            &mut Vec::new(),
        )
        .unwrap();
        let mut commands = Cursor::new(chooser_focus_io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(3, CONTENT_FILE_CHOOSER_FOCUS_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );

        let mut chooser_result_io =
            input_transcript(InputStage::FileChooserResult, &[INPUT_FILE_CHOOSER_OK]);
        probe_input_stream_with_progress(
            &mut chooser_result_io,
            InputStage::FileChooserResult,
            &mut Vec::new(),
        )
        .unwrap();
        let mut commands = Cursor::new(chooser_result_io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(3, CONTENT_FILE_CHOOSER_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );

        let mut retry_io = input_transcript(
            InputStage::Clipboard,
            &[
                INPUT_CLIPBOARD_ARMED,
                INPUT_CLIPBOARD_RETRY,
                INPUT_CLIPBOARD_OK,
            ],
        );
        let mut retry_progress = Vec::new();
        probe_input_stream_with_progress(&mut retry_io, InputStage::Clipboard, &mut retry_progress)
            .unwrap();
        assert_eq!(
            String::from_utf8(retry_progress).unwrap(),
            format!("{INPUT_CLIPBOARD_PUBLIC_ARMED}\n{INPUT_CLIPBOARD_PUBLIC_RETRY}\n")
        );
        let mut retry_commands = Cursor::new(retry_io.output);
        assert_eq!(read_frame(&mut retry_commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut retry_commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"chrome"}]"#
        );
        assert_eq!(
            read_frame(&mut retry_commands).unwrap(),
            execute_command_with_id(3, CHROME_CLIPBOARD_ARM_SCRIPT).unwrap()
        );
        for id in [4, 5] {
            assert_eq!(
                read_frame(&mut retry_commands).unwrap(),
                execute_command_with_id(id, CHROME_CLIPBOARD_SCRIPT).unwrap()
            );
        }
        assert_eq!(
            read_frame(&mut retry_commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );
        assert!(read_frame(&mut retry_commands).is_err());

        let mut exhausted_io = input_transcript(
            InputStage::Clipboard,
            &[
                INPUT_CLIPBOARD_ARMED,
                INPUT_CLIPBOARD_RETRY,
                INPUT_CLIPBOARD_RETRY,
            ],
        );
        let mut exhausted_progress = Vec::new();
        let error = probe_input_stream_with_progress(
            &mut exhausted_io,
            InputStage::Clipboard,
            &mut exhausted_progress,
        )
        .unwrap_err();
        assert!(error.to_string().contains(INPUT_CLIPBOARD_RETRY));
        assert_eq!(
            String::from_utf8(exhausted_progress).unwrap(),
            format!("{INPUT_CLIPBOARD_PUBLIC_ARMED}\n{INPUT_CLIPBOARD_PUBLIC_RETRY}\n")
        );

        let mut clipboard_refocus_arm_io = input_transcript(
            InputStage::ClipboardRefocusArm,
            &[INPUT_CLIPBOARD_REFOCUS_ARMED],
        );
        probe_input_stream(
            &mut clipboard_refocus_arm_io,
            InputStage::ClipboardRefocusArm,
        )
        .unwrap();
        let mut commands = Cursor::new(clipboard_refocus_arm_io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(3, CONTENT_CLIPBOARD_REFOCUS_ARM_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );

        let mut clipboard_refocus_io = input_transcript(
            InputStage::ClipboardRefocus,
            &[INPUT_CLIPBOARD_WINDOW_ARMED],
        );
        probe_input_stream(&mut clipboard_refocus_io, InputStage::ClipboardRefocus).unwrap();
        let mut commands = Cursor::new(clipboard_refocus_io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"content"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(3, CONTENT_CLIPBOARD_REFOCUS_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );

        let mut clipboard_io = input_transcript(
            InputStage::Clipboard,
            &[INPUT_CLIPBOARD_ARMED, INPUT_CLIPBOARD_OK],
        );
        let mut progress = Vec::new();
        probe_input_stream_with_progress(&mut clipboard_io, InputStage::Clipboard, &mut progress)
            .unwrap();
        assert_eq!(
            String::from_utf8(progress).unwrap(),
            format!("{INPUT_CLIPBOARD_PUBLIC_ARMED}\n")
        );
        let mut commands = Cursor::new(clipboard_io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,2,"Marionette:SetContext",{"value":"chrome"}]"#
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(3, CHROME_CLIPBOARD_ARM_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command_with_id(4, CHROME_CLIPBOARD_SCRIPT).unwrap()
        );
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            r#"[0,6,"WebDriver:DeleteSession",{}]"#
        );

        let mut rejected = input_transcript(
            InputStage::Menu,
            &[INPUT_CONTENT_OK, "TD-FIREFOX-INPUT-ERROR:menu"],
        );
        assert!(probe_input_stream(&mut rejected, InputStage::Menu).is_err());
    }

    #[test]
    fn input_scripts_bind_physical_content_and_chrome_events() {
        for event in ["mousemove", "input", "wheel", "contextmenu"] {
            assert!(CONTENT_ARM_SCRIPT.contains(event));
        }
        assert_eq!(CONTENT_ARM_SCRIPT.matches("new AudioContext").count(), 1);
        assert_eq!(CONTENT_ARM_SCRIPT.matches("oscillator.start()").count(), 1);
        assert_eq!(CONTENT_ARM_SCRIPT.matches("oscillator.stop(").count(), 1);
        let trusted = CONTENT_ARM_SCRIPT.find("if (!event.isTrusted").unwrap();
        let context = CONTENT_ARM_SCRIPT.find("new AudioContext({ sampleRate: 48000 })").unwrap();
        let start = CONTENT_ARM_SCRIPT.find("oscillator.start()").unwrap();
        let stop = CONTENT_ARM_SCRIPT
            .find("oscillator.stop(context.currentTime + 1)")
            .unwrap();
        assert!(trusted < context && context < start && start < stop);
        assert!(CONTENT_MENU_SCRIPT.contains("input.value.length <= 4"));
        assert!(CONTENT_MENU_SCRIPT.contains("value => value === \"x\""));
        assert!(CONTENT_MENU_SCRIPT.contains("window.scrollY > 0"));
        assert!(CONTENT_MENU_SCRIPT.contains("audio.starts === 1"));
        assert!(CONTENT_MENU_SCRIPT.contains("audio.ended === 1"));
        assert!(CONTENT_MENU_SCRIPT.contains("audio.closed === 1"));
        assert!(CONTENT_MENU_SCRIPT.contains("audio.rate === 48000"));
        assert!(CONTENT_MENU_SCRIPT.contains("audio.error === \"\""));
        assert!(CHROME_ARM_SCRIPT.contains("contentAreaContextMenu"));
        assert!(CHROME_ARM_SCRIPT.contains("popupshown"));
        assert!(CHROME_ARM_SCRIPT.contains("popuphidden"));
        assert!(CHROME_MENU_SCRIPT.contains("popup.state === \"open\""));
        assert!(CHROME_FINAL_SCRIPT.contains("popup.state === \"closed\""));
        assert!(CONTENT_CLIPBOARD_REFOCUS_ARM_SCRIPT.contains("mousedown"));
        assert!(CONTENT_CLIPBOARD_REFOCUS_ARM_SCRIPT.contains("once: true"));
        assert!(
            CONTENT_CLIPBOARD_REFOCUS_ARM_SCRIPT.contains(INPUT_CLIPBOARD_REFOCUS_ARMED)
        );
        assert!(CONTENT_CLIPBOARD_REFOCUS_SCRIPT.contains("state.down === 1"));
        assert!(CONTENT_CLIPBOARD_REFOCUS_SCRIPT.contains(INPUT_CLIPBOARD_WINDOW_ARMED));
        assert!(CHROME_CLIPBOARD_ARM_SCRIPT.contains("urlbar.focused"));
        assert!(CHROME_CLIPBOARD_ARM_SCRIPT.contains("event.ctrlKey && event.key === \"v\""));
        assert!(CHROME_CLIPBOARD_ARM_SCRIPT.contains(
            "event.clipboardData.getData(\"text/plain\")"
        ));
        assert!(CHROME_CLIPBOARD_ARM_SCRIPT.contains(INPUT_CLIPBOARD_ARMED));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("win.gURLBar.value"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("state.shortcuts >= 1"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("state.shortcuts <= 4"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("retryEvents <= 4"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("state.commandEnds === 1"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("state.commandEnds === 2"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("state.retryFloor === 0"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("if (retry) state.retryFloor"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("state.pastes === state.shortcuts"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("state.emptyPastes === state.pastes"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("valueRepeats >= 1"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("valueRepeats <= state.pastes"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("state.exactPastes <= valueRepeats"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains("!state.unexpected"));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains(
            "value === \"Welcome\".repeat(value.length / \"Welcome\".length)"
        ));
        assert!(CHROME_CLIPBOARD_SCRIPT.contains(INPUT_CLIPBOARD_RETRY));
        assert!(CHROME_CLIPBOARD_ARM_SCRIPT.contains("!focused || !selected"));
        assert!(CHROME_CLIPBOARD_ARM_SCRIPT.contains("event.key === \"Shift\""));
        assert!(CHROME_CLIPBOARD_ARM_SCRIPT.contains("state.commandEnds++"));
        assert!(CONTENT_DOWNLOAD_ARM_SCRIPT.contains("link.download"));
        assert!(CONTENT_DOWNLOAD_ARM_SCRIPT.contains("document.hasFocus()"));
        assert!(CONTENT_DOWNLOAD_ARM_SCRIPT.contains("event.isTrusted"));
        assert!(CONTENT_DOWNLOAD_ARM_SCRIPT.contains("event.preventDefault()"));
        assert!(CONTENT_DOWNLOAD_SCRIPT.contains("state.keys <= 4"));
        assert!(CONTENT_DOWNLOAD_SCRIPT.contains("state.clicks === 1"));
        assert!(CONTENT_DOWNLOAD_SCRIPT.contains("state.commandEnds === 1"));
        assert!(CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT.contains("mousedown"));
        assert!(CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT.contains("once: true"));
        assert!(CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT.contains("event.isTrusted"));
        assert!(CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT
            .contains("Focus Firefox for its native Open File command"));
        assert!(CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT.contains("focus.style.inset = \"0\""));
        assert!(CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT.contains("focus.style.width = \"100%\""));
        assert!(CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT.contains("focus.style.height = \"100%\""));
        assert!(CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT.contains("getBoundingClientRect()"));
        assert!(CONTENT_FILE_CHOOSER_ARM_SCRIPT.contains("refocus.clicks !== 1"));
        assert!(CONTENT_FILE_CHOOSER_ARM_SCRIPT.contains("refocus.button !== 0"));
        assert!(CONTENT_FILE_CHOOSER_ARM_SCRIPT.contains("document.hasFocus()"));
        assert!(CONTENT_FILE_CHOOSER_ARM_SCRIPT.contains("document.activeElement !== focus"));
        assert!(CONTENT_FILE_CHOOSER_ARM_SCRIPT.contains("Number.isFinite(refocus.x)"));
        assert_eq!(
            CONTENT_FILE_CHOOSER_ARM_SCRIPT
                .matches("requestAnimationFrame(")
                .count(),
            2
        );
        assert!(CONTENT_FILE_CHOOSER_FOCUS_SCRIPT.contains("state.downs === 1"));
        assert!(CONTENT_FILE_CHOOSER_FOCUS_SCRIPT.contains("state.clicks === 1"));
        assert!(CONTENT_FILE_CHOOSER_FOCUS_SCRIPT.contains("state.trusted"));
        assert!(CONTENT_FILE_CHOOSER_FOCUS_SCRIPT.contains("state.button === 0"));
        assert!(CONTENT_FILE_CHOOSER_FOCUS_SCRIPT.contains("document.hasFocus()"));
        assert!(CONTENT_FILE_CHOOSER_FOCUS_SCRIPT.contains(INPUT_FILE_CHOOSER_FOCUSED));
        assert!(CONTENT_FILE_CHOOSER_SCRIPT
            .contains("file:///home/td/Downloads/td-firefox-download.txt"));
        assert!(CONTENT_FILE_CHOOSER_SCRIPT
            .contains("document.contentType === \"text/plain\""));
        assert!(CONTENT_FILE_CHOOSER_SCRIPT.contains("document.body.textContent"));
        assert!(CONTENT_FILE_CHOOSER_SCRIPT.contains("TD-FIREFOX-DOWNLOAD-V1\\n"));
        assert!(CONTENT_FILE_CHOOSER_SCRIPT.contains("Date.now() + 20000"));
        for script in [
            CONTENT_MENU_SCRIPT,
            CHROME_MENU_SCRIPT,
            CHROME_FINAL_SCRIPT,
            CONTENT_CLIPBOARD_REFOCUS_SCRIPT,
            CHROME_CLIPBOARD_ARM_SCRIPT,
            CHROME_CLIPBOARD_SCRIPT,
            CONTENT_DOWNLOAD_SCRIPT,
            CONTENT_FILE_CHOOSER_ARM_SCRIPT,
            CONTENT_FILE_CHOOSER_FOCUS_SCRIPT,
            CONTENT_FILE_CHOOSER_SCRIPT,
        ] {
            assert!(script.contains("const expires = Date.now() + 20000"));
            assert!(script.contains("setTimeout(check, 50)"));
        }
        for script in [
            CONTENT_ARM_SCRIPT,
            CHROME_ARM_SCRIPT,
            CONTENT_MENU_SCRIPT,
            CHROME_MENU_SCRIPT,
            CHROME_FINAL_SCRIPT,
            CONTENT_CLIPBOARD_REFOCUS_ARM_SCRIPT,
            CONTENT_CLIPBOARD_REFOCUS_SCRIPT,
            CHROME_CLIPBOARD_ARM_SCRIPT,
            CHROME_CLIPBOARD_SCRIPT,
            CONTENT_DOWNLOAD_ARM_SCRIPT,
            CONTENT_DOWNLOAD_SCRIPT,
            CONTENT_FILE_CHOOSER_REFOCUS_ARM_SCRIPT,
            CONTENT_FILE_CHOOSER_ARM_SCRIPT,
            CONTENT_FILE_CHOOSER_FOCUS_SCRIPT,
            CONTENT_FILE_CHOOSER_SCRIPT,
        ] {
            assert!(execute_command_with_id(5, script).unwrap().len() < MAX_COMMAND_BYTES);
        }
    }

    #[test]
    fn clipboard_result_waits_for_physical_command_boundaries() {
        let classify = |command_ends: usize,
                        retry_floor: usize,
                        shortcuts: usize,
                        pastes: usize,
                        empty: usize,
                        exact: usize,
                        value: &str| {
            let first_bounded =
                command_ends == 1 && retry_floor == 0 && (1..=4).contains(&shortcuts);
            let retry_events = shortcuts.saturating_sub(retry_floor);
            let second_bounded = command_ends == 2
                && (1..=4).contains(&retry_floor)
                && (1..=4).contains(&retry_events);
            let bounded = (first_bounded || second_bounded) && pastes == shortcuts;
            let value_repeats = if !value.is_empty() && value.len().is_multiple_of("Welcome".len())
            {
                let repeats = value.len() / "Welcome".len();
                if value == "Welcome".repeat(repeats) {
                    repeats
                } else {
                    0
                }
            } else {
                0
            };
            let pasted = bounded
                && empty.saturating_add(exact) == pastes
                && value_repeats >= 1
                && value_repeats <= pastes
                && exact <= value_repeats;
            let retry = command_ends == 1
                && retry_floor == 0
                && (1..=4).contains(&shortcuts)
                && pastes == shortcuts
                && empty == pastes
                && exact == 0
                && value == "old";
            if pasted {
                "ok"
            } else if retry {
                "retry"
            } else {
                "pending-or-error"
            }
        };

        assert_eq!(classify(0, 0, 1, 1, 1, 0, "old"), "pending-or-error");
        assert_eq!(classify(1, 0, 1, 1, 1, 0, "old"), "retry");
        // An event arriving after the retry response is not a second command:
        // only the host's following Shift keyup may advance that boundary.
        assert_eq!(classify(1, 1, 2, 2, 1, 1, "Welcome"), "pending-or-error");
        assert_eq!(classify(2, 1, 3, 3, 1, 2, "WelcomeWelcome"), "ok");
        // Firefox may expose an empty DataTransfer while its default action
        // consumes the asynchronous Wayland transfer. The final URL accounts
        // that exact insertion even when only the later event exposes bytes.
        assert_eq!(classify(2, 1, 2, 2, 1, 1, "WelcomeWelcome"), "ok");
        assert_eq!(
            classify(2, 1, 6, 6, 1, 5, &"Welcome".repeat(5)),
            "pending-or-error"
        );
    }

    #[test]
    fn download_result_waits_for_the_physical_command_boundary() {
        let accepted = |keys: usize, clicks: usize, command_ends: usize, trusted: bool| {
            (1..=4).contains(&keys) && clicks == 1 && command_ends == 1 && trusted
        };

        assert!(!accepted(1, 1, 0, true));
        assert!(accepted(1, 1, 1, true));
        // A delayed fifth Enter precedes the separately injected Shift keyup,
        // so it is visible before the command can complete.
        assert!(!accepted(5, 1, 0, true));
        assert!(!accepted(5, 1, 1, true));
        assert!(!accepted(1, 2, 1, true));
        assert!(!accepted(1, 1, 1, false));
    }

    #[test]
    fn download_probe_requires_one_stable_exact_regular_file() {
        let directory = download_test_directory();
        let owner = fs::metadata(&directory).unwrap();
        let path = directory.join(DOWNLOAD_NAME);
        fs::write(&path, DOWNLOAD_BYTES).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        validate_download(&directory, DOWNLOAD_BYTES, owner.uid(), owner.gid()).unwrap();

        fs::write(&path, b"TD-FIREFOX-DOWNLOAD-V0\n").unwrap();
        assert!(validate_download(&directory, DOWNLOAD_BYTES, owner.uid(), owner.gid(),).is_err());
        fs::write(&path, DOWNLOAD_BYTES).unwrap();

        let duplicate = directory.join("td-firefox-download.txt.part");
        fs::write(&duplicate, DOWNLOAD_BYTES).unwrap();
        assert!(validate_download(&directory, DOWNLOAD_BYTES, owner.uid(), owner.gid(),).is_err());
        fs::remove_file(&duplicate).unwrap();

        let hardlink = directory.join("unrelated-hardlink");
        fs::hard_link(&path, &hardlink).unwrap();
        assert!(validate_download(&directory, DOWNLOAD_BYTES, owner.uid(), owner.gid(),).is_err());
        fs::remove_file(&hardlink).unwrap();

        fs::remove_file(&path).unwrap();
        fs::write(directory.join("elsewhere"), DOWNLOAD_BYTES).unwrap();
        symlink("elsewhere", &path).unwrap();
        assert!(validate_download(&directory, DOWNLOAD_BYTES, owner.uid(), owner.gid(),).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exact_support_report_requires_nested_filters_for_live_roles() {
        let report = parse_report(GOOD_REPORT).unwrap();
        validate_report(&report, &good_sandboxes()).unwrap();

        for replacement in [
            ("protocol=wayland", "protocol=x11"),
            ("effective=6", "effective=0"),
            ("userns=false", "userns=true"),
            ("content:11.12", "content:"),
            ("rdd:14,utility:15", "rdd:,utility:"),
        ] {
            let altered = GOOD_REPORT.replace(replacement.0, replacement.1);
            let parsed = parse_report(&altered).unwrap();
            assert!(
                validate_report(&parsed, &good_sandboxes()).is_err(),
                "accepted {altered}"
            );
        }

        let mut weak = good_sandboxes();
        weak.get_mut(2).unwrap().filters = 1;
        assert!(validate_report(&report, &weak).is_err());

        let unrelated_utility = GOOD_REPORT.replace("rdd:14", "rdd:");
        assert!(validate_report(
            &parse_report(&unrelated_utility).unwrap(),
            &good_sandboxes(),
        )
        .is_err());

        let media_utility = unrelated_utility.replace("media=none", "media=15");
        validate_report(&parse_report(&media_utility).unwrap(), &good_sandboxes()).unwrap();
        assert!(parse_report(&media_utility.replace("media=15", "media=16")).is_ok());
        assert!(validate_report(
            &parse_report(&media_utility.replace("media=15", "media=16")).unwrap(),
            &good_sandboxes(),
        )
        .is_err());
        for malformed in ["media=0", "media=15.15", "media="] {
            assert!(parse_report(&GOOD_REPORT.replace("media=none", malformed)).is_err());
        }
    }

    #[test]
    fn report_parser_refuses_reordered_duplicate_and_escaped_fields() {
        assert!(parse_report(&GOOD_REPORT.replace(
            "|protocol=wayland|compositor=WebRender (Software)",
            "|compositor=WebRender (Software)|protocol=wayland"
        ))
        .is_err());
        assert!(parse_report(&GOOD_REPORT.replace("utility:15", "rdd:15")).is_err());
        let response =
            format!("{EXECUTE_RESPONSE_PREFIX}{GOOD_REPORT}\\n{EXECUTE_RESPONSE_SUFFIX}");
        let value = response
            .strip_prefix(EXECUTE_RESPONSE_PREFIX)
            .and_then(|rest| rest.strip_suffix(EXECUTE_RESPONSE_SUFFIX))
            .unwrap();
        assert!(value.contains('\\'));
    }
}
