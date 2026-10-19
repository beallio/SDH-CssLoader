import { callBackend, callBackendAs, isString, toast } from "../../python";

export async function booleanStoreRead(key: string) {
  const deckyRes = await callBackendAs(isString, "store_read", key);
  if (!deckyRes.success) {
    toast(`Error fetching ${key}`, deckyRes.result);
    return false;
  }
  return deckyRes.result === "1" || deckyRes.result === "true";
}

export async function booleanStoreWrite(key: string, value: boolean) {
  const deckyRes = await callBackend("store_write", key, value ? "1" : "0");
  if (!deckyRes.success) {
    toast(`Error setting ${key}`, deckyRes.result);
  }
}

export async function stringStoreRead(key: string) {
  const deckyRes = await callBackendAs(isString, "store_read", key);
  if (!deckyRes.success) {
    toast(`Error fetching ${key}`, deckyRes.result);
    return "";
  }
  return deckyRes.result;
}
export async function stringStoreWrite(key: string, value: string) {
  const deckyRes = await callBackend("store_write", key, value);
  if (!deckyRes.success) {
    toast(`Error setting ${key}`, deckyRes.result);
  }
}
