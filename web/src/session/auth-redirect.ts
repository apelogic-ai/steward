const exactReturnPaths = new Set(["/connections", "/envelopes", "/envelopes/new", "/runs", "/settings"]);

export function loginReturnTo(pathname: string): string {
  if (exactReturnPaths.has(pathname)) return pathname;
  if (pathname.startsWith("/runs/")) return "/runs";
  return "/envelopes";
}

export function authStartPath(pathname: string): string {
  return `/admin/auth/login?returnTo=${encodeURIComponent(loginReturnTo(pathname))}`;
}
