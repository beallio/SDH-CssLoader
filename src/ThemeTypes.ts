import { MinimalCSSThemeInfo } from "./apiTypes";

export interface Theme {
  id: string;
  enabled: boolean; // used to be called checked
  name: string;
  display_name: string;
  author: string;
  bundled: boolean; // deprecated
  require: number;
  version: string;
  patches: Patch[];
  dependencies: string[];
  flags: Flags[];
}

export interface ThemePatchComponent {
  name: string;
  on: string;
  type: string;
  value: string;
}

export interface Patch {
  default: string;
  name: string;
  type: "dropdown" | "checkbox" | "slider" | "none";
  value: string;
  options: string[];
  components: ThemePatchComponent[];
}

export enum Flags {
  "isPreset" = "PRESET",
  "dontDisableDeps" = "KEEP_DEPENDENCIES",
  "optionalDeps" = "OPTIONAL_DEPENDENCIES",
  "navPatch" = "REQUIRE_NAV_PATCH",
}

export type LocalThemeStatus = "installed" | "outdated" | "local";
export type UpdateStatus = [string, LocalThemeStatus, false | MinimalCSSThemeInfo];

type ThemeErrorTitle = string;
type ThemeErrorDescription = string;
export type ThemeError = [ThemeErrorTitle, ThemeErrorDescription];

type Fields = Record<string, unknown>;
const isFields = (value: unknown): value is Fields =>
  typeof value === "object" && value !== null && !Array.isArray(value);
const isStringList = (value: unknown): value is string[] =>
  Array.isArray(value) && value.every((item) => typeof item === "string");
const hasStrings = (value: Fields, keys: string[]) => keys.every((key) => typeof value[key] === "string");

function isPatch(value: unknown): value is Patch {
  return (
    isFields(value) &&
    hasStrings(value, ["default", "name", "type", "value"]) &&
    ["dropdown", "checkbox", "slider", "none"].includes(String(value.type)) &&
    isStringList(value.options) &&
    Array.isArray(value.components) &&
    value.components.every((c) => isFields(c) && hasStrings(c, ["name", "on", "type", "value"]))
  );
}

export function isTheme(value: unknown): value is Theme {
  return (
    isFields(value) &&
    hasStrings(value, ["id", "name", "display_name", "author", "version"]) &&
    typeof value.enabled === "boolean" &&
    typeof value.bundled === "boolean" &&
    typeof value.require === "number" &&
    Array.isArray(value.patches) &&
    value.patches.every(isPatch) &&
    isStringList(value.dependencies) &&
    isStringList(value.flags)
  );
}

export const isThemeError = (value: unknown): value is ThemeError =>
  Array.isArray(value) && value.length === 2 && value.every((item) => typeof item === "string");
