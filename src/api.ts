import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Kind = "image" | "video";
export type Media = {
  id: number; path: string; name: string; kind: Kind; bytes: number; modified: number;
  date: number; width: number | null; height: number | null; duration: number | null;
  favorite: boolean; rotation: number; thumbnail: string | null;
  /** 0 queued, 1 ready, 2 undecodable */
  state: 0 | 1 | 2;
};
export type Detail = Media & {
  dir: string; added: number; taken: number | null; camera: string | null; lens: string | null;
  iso: number | null; aperture: number | null; exposure: string | null; focal: number | null;
  lat: number | null; lon: number | null; tags: string[]; albums: { id: number; name: string }[];
};
export type Filter = {
  search: string; kind: "all" | Kind;
  scope: "all" | "favorites" | "folder" | "album" | "tag" | "smart";
  folderId?: number; dir?: string; albumId?: number; tag?: string; smart?: "recent" | "untagged" | "gps";
};
export type Sort = { key: "date" | "name" | "added" | "size" | "kind"; desc: boolean };
export type Group = "none" | "day" | "month" | "year";
export type Layout = { total: number; groups: { key: string; count: number }[] };
export type Folder = { id: number; path: string; name: string; count: number; available: boolean; lastScan: number | null };
export type Named = { id: number; name: string; count: number };
export type Stats = { total: number; images: number; videos: number; favorites: number; bytes: number; pending: number; failed: number };
export type Progress = {
  running: boolean; cancelled: boolean; phase: "idle" | "discover" | "process";
  folder: string; discovered: number; processed: number; total: number; failed: number;
};
export type DupGroup = { bytes: number; items: Media[] };
export type AppInfo = {
  version: string; ffmpeg: boolean; ffprobe: boolean; dataDir: string;
  thumbnails: { bytes: number; files: number }; previews: { bytes: number; files: number };
};
export type Settings = Record<string, string>;

export const src = (path: string) => convertFileSrc(path);

export const api = {
  ready: () => invoke<void>("app_ready"),
  info: () => invoke<AppInfo>("app_info"),
  videoSupported: () => invoke<boolean>("video_supported"),
  settings: () => invoke<Settings>("get_settings"),
  setSetting: (key: string, value: string) => invoke<void>("set_setting", { key, value }),
  folders: () => invoke<Folder[]>("list_folders"),
  dirs: (folderId: number) => invoke<{ dir: string; count: number }[]>("list_dirs", { folderId }),
  addFolder: (path: string) => invoke<number>("add_folder", { path }),
  removeFolder: (id: number) => invoke<void>("remove_folder", { id }),
  scan: (folderId?: number) => invoke<void>("start_scan", { folderId: folderId ?? null }),
  cancelScan: () => invoke<void>("cancel_scan"),
  scanStatus: () => invoke<Progress>("scan_status"),
  stats: () => invoke<Stats>("library_stats"),
  layout: (filter: Filter, group: Group, desc: boolean) => invoke<Layout>("query_layout", { filter, group, desc }),
  media: (filter: Filter, sort: Sort, offset: number, limit: number) => invoke<Media[]>("query_media", { filter, sort, offset, limit }),
  ids: (filter: Filter, sort: Sort, offset: number, limit: number) => invoke<number[]>("query_ids", { filter, sort, offset, limit }),
  detail: (id: number) => invoke<Detail>("get_media", { id }),
  favorite: (ids: number[], value: boolean) => invoke<void>("set_favorite", { ids, value }),
  rotate: (ids: number[], delta: number) => invoke<void>("rotate_media", { ids, delta }),
  albums: () => invoke<Named[]>("list_albums"),
  createAlbum: (name: string) => invoke<number>("create_album", { name }),
  renameAlbum: (id: number, name: string) => invoke<void>("rename_album", { id, name }),
  deleteAlbum: (id: number) => invoke<void>("delete_album", { id }),
  albumAdd: (albumId: number, ids: number[]) => invoke<number>("album_add", { albumId, ids }),
  albumRemove: (albumId: number, ids: number[]) => invoke<void>("album_remove", { albumId, ids }),
  tags: () => invoke<Named[]>("list_tags"),
  addTag: (ids: number[], tag: string) => invoke<void>("add_tag", { ids, tag }),
  removeTag: (ids: number[], tag: string) => invoke<void>("remove_tag", { ids, tag }),
  preview: (id: number) => invoke<string>("ensure_preview", { id }),
  videoUrl: (id: number) => invoke<string>("video_url", { id }),
  videoPlayback: (id: number) => invoke<string>("ensure_video_playback", { id }),
  cancelVideoPlayback: () => invoke<void>("cancel_video_playback"),
  reveal: (path: string) => invoke<void>("reveal_in_folder", { path }),
  openExternal: (path: string) => invoke<void>("open_external", { path }),
  duplicates: () => invoke<DupGroup[]>("find_duplicates"),
  cancelDuplicates: () => invoke<void>("cancel_duplicates"),
  excluded: () => invoke<string[]>("list_excluded"),
  addExcluded: (entry: string) => invoke<number>("add_excluded", { entry }),
  removeExcluded: (entry: string) => invoke<void>("remove_excluded", { entry }),
  rebuildThumbs: () => invoke<void>("rebuild_thumbnails"),
  clearPreviews: () => invoke<number>("clear_preview_cache"),
  backup: (dest: string) => invoke<void>("backup_catalog", { dest }),
  restore: (src: string) => invoke<void>("restore_catalog", { src }),
};

export const on = <T>(event: string, cb: (payload: T) => void): Promise<UnlistenFn> =>
  listen<T>(event, (e) => cb(e.payload));

/** Invoke errors arrive as plain strings from Rust; normalise anything else. */
export const errText = (e: unknown): string =>
  typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
