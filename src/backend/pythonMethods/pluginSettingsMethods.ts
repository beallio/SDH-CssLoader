import { callBackend, callBackendAs, isBoolean, isString, storeRead, toast } from "../../python";
import { globalState } from "../pythonRoot";
import { booleanStoreRead, stringStoreRead } from "./storeUtils";

export function enableServer() {
  return callBackend("enable_server");
}

export async function getServerState() {
  const deckyRes = await callBackendAs(isBoolean, "get_server_state");
  if (!deckyRes.success) {
    toast("Error fetching server state", deckyRes.result);
    return false;
  }
  return deckyRes.result;
}

export async function getWatchState() {
  const deckyRes = await callBackendAs(isBoolean, "get_watch_state");
  if (!deckyRes.success) {
    toast("Error fetching watch state", deckyRes.result);
    return false;
  }
  return deckyRes.result;
}

export async function getBetaTranslationsState() {
  return stringStoreRead("beta_translations");
}

export function toggleWatchState(bool: boolean, onlyThisSession: boolean = false) {
  return callBackend("toggle_watch_state", bool, onlyThisSession);
}

// Todo: when i rewrite store interop, move this
export function setHiddenMotd(id: string) {
  return callBackend("store_write", "hiddenMotd", id);
}
export function getHiddenMotd() {
  return callBackendAs(isString, "store_read", "hiddenMotd");
}

export function fetchClassMappings() {
  return callBackend("fetch_class_mappings");
}
