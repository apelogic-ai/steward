"use strict";

const path = window.location.pathname;
const pages = Array.from(document.querySelectorAll("[data-page]"));
const links = Array.from(document.querySelectorAll("[data-route]"));

for (const page of pages) {
  page.hidden = page.dataset.page !== path;
}
for (const link of links) {
  if (link.dataset.route === path) {
    link.setAttribute("aria-current", "page");
  }
}
