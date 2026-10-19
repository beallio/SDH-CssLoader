// Code from https://github.com/NGnius/PowerTools/blob/dev/src/python.ts
import { ServerAPI } from "decky-frontend-lib";
import { CssLoaderState } from "./state";
import { Theme, ThemeError } from "./ThemeTypes";
import { bulkThemeUpdateCheck } from "./logic/bulkThemeUpdateCheck";

export var server: ServerAPI | undefined = undefined;
export var globalState: CssLoaderState | undefined = undefined;

export function setServer(s: ServerAPI) {
  server = s;
}
export function setStateClass(s: CssLoaderState): void {
  globalState = s;
}

type BackendResult<T> = { success: true; result: T } | { success: false; result: string };
type DeckyBackend = { call: (method: string, ...args: unknown[]) => Promise<unknown> };
declare global {
  interface Window {
    __DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit: {
      connect: (version: number, pluginName: string) => DeckyBackend;
    };
  }
}
let backend: DeckyBackend | undefined;

// The fork's native backend refuses legacy callPluginMethod calls; use the API v1 positional route.
export async function callBackend<T = unknown>(method: string, ...args: unknown[]): Promise<BackendResult<T>> {
  backend ??= window.__DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit.connect(1, "CSS Loader");
  try {
    // Unchecked: the backend owns the result shape, as it did with callPluginMethod.
    return { success: true, result: (await backend.call(method, ...args)) as T };
  } catch (e) {
    return { success: false, result: String(e) };
  }
}

export async function openFilePicker(path: string) {
  return await server!.openFilePicker(path, true);
}

export function fetchThemePath() {
  return callBackend<string>("fetch_theme_path");
}

export function resolve(promise: Promise<any>, setter: any) {
  (async function () {
    let data = await promise;
    if (data.success) {
      console.debug("Got resolved", data, "promise", promise);
      setter(data.result);
    } else {
      console.warn("Resolve failed:", data, "promise", promise);
    }
  })();
}

export function execute(promise: Promise<any>) {
  (async function () {
    let data = await promise;
    if (data.success) {
      console.debug("Got executed", data, "promise", promise);
    } else {
      console.warn("Execute failed:", data, "promise", promise);
    }
  })();
}

export async function scheduleCheckForUpdates() {
  const setGlobalState = globalState!.setGlobalState.bind(globalState);
  function recursiveCheck() {
    const timeout = setTimeout(async () => {
      // Putting this in the function as im not sure the value would update otherwise
      const { nextUpdateCheckTime } = globalState!.getPublicState();
      if (!(new Date().valueOf() > nextUpdateCheckTime)) {
        recursiveCheck();
        return;
      }
      // After testing, it appears that, if there is no wifi, bulkThemeUpdateCheck returns an empty array, this is okay, the try catch is just for extra safety
      try {
        const data = await bulkThemeUpdateCheck();
        if (data) {
          // 24hrs from now
          setGlobalState("updateStatuses", data);
        }
        setGlobalState("nextUpdateCheckTime", new Date().valueOf() + 24 * 60 * 60 * 1000);
      } catch (err) {
        console.log("Error Checking For Theme Updates", err);
      }
      recursiveCheck();
    }, 5 * 60 * 1000);
    setGlobalState("updateCheckTimeout", timeout);
  }
  // Initially setting it
  // 24hrs from now
  setGlobalState("nextUpdateCheckTime", new Date().valueOf() + 24 * 60 * 60 * 1000);
  recursiveCheck();
}

export async function changePreset(themeName: string, themeList: Theme[]) {
  return new Promise(async (resolve) => {
    const { selectedPreset } = globalState!.getPublicState();

    if (selectedPreset) {
      // If you already have a preset enabled, since all currently enabled themes are part of that preset, you only need to disable it, not every theme
      await setThemeState(selectedPreset!.name, false);
    } else {
      // On the contrary, if you have no preset, you still do have to disable the current themes and then enable the preset
      await Promise.all(
        themeList.filter((e) => e.enabled).map((e) => setThemeState(e.name, false))
      );
    }

    if (themeName !== "None") {
      await setThemeState(themeName, true);
    }
    resolve(true);
  });
}

export async function getInstalledThemes(): Promise<any> {
  const setGlobalState = globalState!.setGlobalState.bind(globalState);
  const errorRes = await callBackend<{ fails: ThemeError[] }>("get_last_load_errors");
  if (errorRes.success) {
    setGlobalState("themeErrors", errorRes.result.fails);
  }
  const themeRes = await callBackend<Theme[]>("get_themes");
  if (themeRes.success) {
    setGlobalState("localThemeList", themeRes.result);
    return themeRes.result;
  }
}

export async function reloadBackend(): Promise<void> {
  return callBackend<{ fails: ThemeError[] }>("reset").then((res) => {
    return getInstalledThemes();
  });
}

export function getThemes() {
  return callBackend<Theme[]>("get_themes");
}

export function setThemeState(
  name: string,
  state: boolean,
  set_deps?: boolean,
  set_deps_value?: boolean
): Promise<any> {
  return callBackend("set_theme_state", name, state, set_deps ?? true, set_deps_value ?? true);
}

export function setPatchOfTheme(themeName: string, patchName: string, value: string): Promise<any> {
  return callBackend("set_patch_of_theme", themeName, patchName, value);
}

export function setComponentOfThemePatch(
  themeName: string,
  patchName: string,
  componentName: string,
  value: string
): Promise<any> {
  return callBackend("set_component_of_theme_patch", themeName, patchName, componentName, value);
}

export function toast(title: string, message: string) {
  // This is a weirdo self-invoking function, but it works.
  return (() => {
    try {
      return server?.toaster.toast({
        title: title,
        body: message,
        duration: 8000,
      });
    } catch (e) {
      console.log("CSSLoader Toaster Error", e);
    }
  })();
}

export function downloadThemeFromUrl(themeId: string): Promise<any> {
  const { apiUrl } = globalState!.getPublicState();
  return callBackend("download_theme_from_url", themeId, apiUrl);
}

export function deleteTheme(themeName: string): Promise<any> {
  return callBackend("delete_theme", themeName);
}

export function storeRead(key: string) {
  return callBackend<string>("store_read", key);
}

export function storeWrite(key: string, value: string) {
  return callBackend("store_write", key, value);
}

export function enableServer() {
  return callBackend("enable_server");
}
export function getServerState() {
  return callBackend<boolean>("get_server_state");
}

export function getBackendVersion(): Promise<any> {
  const setGlobalState = globalState!.setGlobalState.bind(globalState);
  return callBackend<number>("get_backend_version").then((data) => {
    if (data.success) {
      setGlobalState("backendVersion", data.result);
    }
    return;
  });
}

export function dummyFunction() {
  return callBackend<boolean>("dummy_function");
}

export function genericGET(fetchUrl: string, authToken?: string | undefined) {
  return server!
    .fetchNoCors<Response>(`${fetchUrl}`, {
      method: "GET",
      headers: authToken
        ? {
            Authorization: `Bearer ${authToken}`,
          }
        : {},
    })
    .then((deckyRes) => {
      if (deckyRes.success) {
        return deckyRes.result;
      }
      throw new Error(`Fetch not successful!`);
    })
    .then((res) => {
      if (res.status >= 200 && res.status <= 300 && res.body) {
        // @ts-ignore
        return JSON.parse(res.body || "");
      }
      throw new Error(`Res not OK!, code ${res.status}`);
    })
    .then((json) => {
      if (json) {
        return json;
      }
      throw new Error(`No json returned!`);
    })
    .catch((err) => {
      console.error(`Error fetching ${fetchUrl}`, err);
    });
}

export function unpinTheme(id: string) {
  const { unpinnedThemes } = globalState!.getPublicState();
  const setGlobalState = globalState!.setGlobalState.bind(globalState);
  const newArr = [...unpinnedThemes, id];
  setGlobalState("unpinnedThemes", newArr);
  return storeWrite("unpinnedThemes", JSON.stringify(newArr));
}

export function pinTheme(id: string) {
  const { unpinnedThemes } = globalState!.getPublicState();
  const setGlobalState = globalState!.setGlobalState.bind(globalState);
  const newArr = unpinnedThemes.filter((e) => e !== id);
  setGlobalState("unpinnedThemes", newArr);
  return storeWrite("unpinnedThemes", JSON.stringify(newArr));
}

export function generatePreset(name: string) {
  return callBackend("generate_preset_theme", name);
}

export function generatePresetFromThemeNames(name: string, themeNames: string[]) {
  return callBackend("generate_preset_theme_from_theme_names", name, themeNames);
}
