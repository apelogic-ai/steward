export const ADMIN_SETUP_DISMISSED_KEY = "steward.ui.admin-setup-dismissed";
export const ADMIN_SETUP_PREFERENCE_EVENT = "hypershell:admin-setup-preference";

export function adminSetupDismissed(): boolean {
  return window.localStorage.getItem(ADMIN_SETUP_DISMISSED_KEY) === "true";
}

export function adminSetupVisibleOnServer(): boolean {
  return false;
}

export function subscribeToAdminSetupPreference(onChange: () => void): () => void {
  window.addEventListener(ADMIN_SETUP_PREFERENCE_EVENT, onChange);
  return () => window.removeEventListener(ADMIN_SETUP_PREFERENCE_EVENT, onChange);
}

export function setAdminSetupDismissed(dismissed: boolean): void {
  if (dismissed) window.localStorage.setItem(ADMIN_SETUP_DISMISSED_KEY, "true");
  else window.localStorage.removeItem(ADMIN_SETUP_DISMISSED_KEY);
  window.dispatchEvent(new CustomEvent(ADMIN_SETUP_PREFERENCE_EVENT, { detail: { dismissed } }));
}
