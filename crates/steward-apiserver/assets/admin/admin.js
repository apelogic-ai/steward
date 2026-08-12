"use strict";

const tabs = Array.from(document.querySelectorAll("[data-surface]"));
const panels = Array.from(document.querySelectorAll("[data-panel]"));
const operator = document.querySelector(".operator");
const operatorName = document.querySelector("#operator");
const fatal = document.querySelector("#fatal");

function selectSurface(name) {
  for (const tab of tabs) {
    const active = tab.dataset.surface === name;
    tab.setAttribute("aria-selected", String(active));
    tab.setAttribute("tabindex", active ? "0" : "-1");
    if (active) {
      tab.setAttribute("aria-current", "page");
    } else {
      tab.removeAttribute("aria-current");
    }
  }
  for (const panel of panels) {
    panel.hidden = panel.dataset.panel !== name;
  }
}

for (const tab of tabs) {
  tab.addEventListener("click", () => selectSurface(tab.dataset.surface));
  tab.addEventListener("keydown", (event) => {
    const current = tabs.indexOf(tab);
    let target = null;
    if (event.key === "ArrowLeft") {
      target = tabs[(current - 1 + tabs.length) % tabs.length];
    } else if (event.key === "ArrowRight") {
      target = tabs[(current + 1) % tabs.length];
    } else if (event.key === "Home") {
      target = tabs[0];
    } else if (event.key === "End") {
      target = tabs[tabs.length - 1];
    }
    if (target) {
      event.preventDefault();
      selectSurface(target.dataset.surface);
      window.location.hash = target.dataset.surface;
      target.focus();
    }
  });
}

function selectSurfaceFromHash() {
  const requested = window.location.hash.slice(1) || "approvals";
  if (tabs.some((tab) => tab.dataset.surface === requested)) {
    selectSurface(requested);
  }
}

window.addEventListener("hashchange", selectSurfaceFromHash);
selectSurfaceFromHash();

async function loadBootstrap() {
  try {
    const response = await fetch("/admin/api/v1/bootstrap", {
      headers: { Accept: "application/json" },
      cache: "no-store",
    });
    if (!response.ok) {
      throw new Error("administrator bootstrap rejected");
    }
    const value = await response.json();
    if (
      value.apiVersion !== "steward.admin/v1" ||
      typeof value.actor !== "string" ||
      value.actor.length === 0 ||
      !Array.isArray(value.surfaces) ||
      value.surfaces.length !== tabs.length ||
      value.surfaces.some((surface) =>
        !tabs.some((tab) => tab.dataset.surface === surface)
      )
    ) {
      throw new Error("administrator bootstrap contract mismatch");
    }
    const allowed = new Set(value.surfaces);
    for (const tab of tabs) {
      tab.hidden = !allowed.has(tab.dataset.surface);
    }
    operatorName.textContent = value.actor;
    operator.dataset.ready = "true";
  } catch (_error) {
    operatorName.textContent = "Administrator access unavailable";
    operator.dataset.ready = "false";
    fatal.hidden = false;
  }
}

void loadBootstrap();
