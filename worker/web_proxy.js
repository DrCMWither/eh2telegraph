/*
 * Cloudflare Workers Telegraph proxy.
 * Deploy it and set the `KEY` environment variable.
 */

const RESPONSE_HEADERS = {
  Server: "web-proxy",
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, HEAD, OPTIONS",
  "Access-Control-Allow-Headers":
    "X-Authorization, X-Target-URL, Content-Type, Accept, Accept-Language, Referer",
};

const PRIVATE_ALLOWED_HOSTS = new Set([
  "api.telegram.org",
  "api.telegra.ph",
  "telegra.ph",
  "edit.telegra.ph",
  "i.nhentai.net",
  "i2.nhentai.net",
  "i3.nhentai.net",
  "ehgt.org",
  "e-hentai.org",
  "exhentai.org",
  // Pixiv AJAX API. This path is protected by X-Authorization.
  "www.pixiv.net",
]);

const IMAGE_ALLOWED_HOSTS = new Set([
  "i.nhentai.net",
  "i2.nhentai.net",
  "i3.nhentai.net",
  "t.nhentai.net",
  "ehgt.org",
  "e-hentai.org",
  "exhentai.org",
  // Pixiv image CDN. Authentication cookies are never forwarded here.
  "i.pximg.net",
]);

const MAX_IMAGE_BYTES = 50 * 1024 * 1024;
const PIXIV_WEB_HOST = "www.pixiv.net";
const PIXIV_REFERER = "https://www.pixiv.net/";
const PIXIV_ILLUST_API_PATH = /^\/ajax\/illust\/\d+(?:\/pages)?$/;

function text(status, body) {
  return new Response(body, {
    status,
    headers: {
      ...RESPONSE_HEADERS,
      "Content-Type": "text/plain; charset=utf-8",
      "X-Content-Type-Options": "nosniff",
    },
  });
}

function isAllowedMethod(method) {
  return method === "GET" || method === "POST" || method === "HEAD";
}

function validateHttpsUrl(url) {
  if (url.protocol !== "https:") {
    return "only https is allowed";
  }
  if (url.username || url.password) {
    return "url credentials are not allowed";
  }
  return null;
}

function isAllowedPrivateHost(hostname) {
  return PRIVATE_ALLOWED_HOSTS.has(hostname.toLowerCase());
}

function isAllowedImageHost(hostname) {
  const host = hostname.toLowerCase();
  if (IMAGE_ALLOWED_HOSTS.has(host)) {
    return true;
  }
  return host.endsWith(".hath.network");
}

function copyHeaderIfPresent(source, destination, name) {
  const value = source.get(name);
  if (value) {
    destination.set(name, value);
  }
}

function buildPrivateHeaders(request, targetUrl) {
  const out = new Headers();

  for (const name of [
    "Content-Type",
    "Accept",
    "Accept-Language",
    "User-Agent",
  ]) {
    copyHeaderIfPresent(request.headers, out, name);
  }

  const host = targetUrl.hostname.toLowerCase();
  if (host === PIXIV_WEB_HOST) {
    // Only Pixiv receives the Pixiv session cookie. Do not forward cookies to
    // the other allow-listed services by accident.
    copyHeaderIfPresent(request.headers, out, "Cookie");
    copyHeaderIfPresent(request.headers, out, "Referer");

    if (!out.has("Referer")) {
      out.set("Referer", PIXIV_REFERER);
    }
  }

  return out;
}

function buildImageHeaders(targetUrl) {
  const headers = new Headers();
  headers.set(
    "User-Agent",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
  );

  const host = targetUrl.hostname.toLowerCase();
  if (host.endsWith("nhentai.net")) {
    headers.set("Referer", "https://nhentai.net/");
  } else if (host === "i.pximg.net") {
    headers.set("Referer", PIXIV_REFERER);
  } else if (
    host === "ehgt.org" ||
    host === "e-hentai.org" ||
    host === "exhentai.org" ||
    host.endsWith(".hath.network")
  ) {
    headers.set("Referer", "https://exhentai.org/");
  }

  return headers;
}

function buildSafeResponseHeaders(upstream, extra = {}) {
  const headers = new Headers();
  const contentType = upstream.headers.get("Content-Type");
  if (contentType) {
    headers.set("Content-Type", contentType);
  }

  const contentLength = upstream.headers.get("Content-Length");
  if (contentLength) {
    headers.set("Content-Length", contentLength);
  }

  headers.set("Access-Control-Allow-Origin", "*");
  headers.set("X-Content-Type-Options", "nosniff");

  for (const [key, value] of Object.entries(extra)) {
    headers.set(key, value);
  }

  return headers;
}

async function handlePrivateProxy(request, env) {
  if (!env.KEY || request.headers.get("X-Authorization") !== env.KEY) {
    return text(401, "unauthorized");
  }

  if (!isAllowedMethod(request.method)) {
    return text(405, "method not allowed");
  }

  const target = request.headers.get("X-Target-URL");
  if (!target) {
    return text(400, "missing X-Target-URL");
  }

  let targetUrl;
  try {
    targetUrl = new URL(target);
  } catch {
    return text(400, "invalid target url");
  }

  const invalidReason = validateHttpsUrl(targetUrl);
  if (invalidReason) {
    return text(403, invalidReason);
  }

  if (!isAllowedPrivateHost(targetUrl.hostname)) {
    return text(403, `target host is not allowed: ${targetUrl.hostname}`);
  }

  const isPixivTarget =
    targetUrl.hostname.toLowerCase() === PIXIV_WEB_HOST;

  if (isPixivTarget) {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return text(405, "pixiv API proxy only allows GET and HEAD");
    }
    if (!PIXIV_ILLUST_API_PATH.test(targetUrl.pathname) || targetUrl.search) {
      return text(403, "pixiv API path is not allowed");
    }
  }

  const init = {
    method: request.method,
    headers: buildPrivateHeaders(request, targetUrl),
    // Do not let an authenticated Pixiv request redirect its Cookie to a
    // different host. The two allow-listed AJAX endpoints return JSON directly.
    redirect: isPixivTarget ? "manual" : "follow",
  };

  if (request.method !== "GET" && request.method !== "HEAD") {
    init.body = request.body;
  }

  const upstream = await fetch(targetUrl.toString(), init);

  if (isPixivTarget && upstream.status >= 300 && upstream.status < 400) {
    return text(502, "unexpected pixiv redirect");
  }

  const headers = buildSafeResponseHeaders(upstream);

  return new Response(upstream.body, {
    status: upstream.status,
    headers,
  });
}

async function handlePublicImageProxy(request) {
  if (request.method !== "GET" && request.method !== "HEAD") {
    return text(405, "method not allowed");
  }

  const reqUrl = new URL(request.url);
  const target = reqUrl.searchParams.get("u");
  if (!target) {
    return text(400, "missing u");
  }

  let targetUrl;
  try {
    targetUrl = new URL(target);
  } catch {
    return text(400, "invalid image url");
  }

  const invalidReason = validateHttpsUrl(targetUrl);
  if (invalidReason) {
    return text(403, invalidReason);
  }

  if (!isAllowedImageHost(targetUrl.hostname)) {
    return text(403, `forbidden host: ${targetUrl.hostname}`);
  }

  const upstream = await fetch(targetUrl.toString(), {
    method: request.method,
    headers: buildImageHeaders(targetUrl),
    redirect: "follow",
    cf: {
      cacheTtl: 86400,
      cacheEverything: true,
      cacheKey: targetUrl.toString(),
    },
  });

  if (!upstream.ok) {
    return text(upstream.status, `upstream ${upstream.status}`);
  }

  const len = upstream.headers.get("Content-Length");
  if (len && Number(len) > MAX_IMAGE_BYTES) {
    return text(413, "image too large");
  }

  const contentType = upstream.headers.get("Content-Type") || "";
  if (contentType && !contentType.toLowerCase().startsWith("image/")) {
    return text(415, "upstream is not an image");
  }

  const headers = buildSafeResponseHeaders(upstream, {
    "Cache-Control": "public, max-age=86400",
  });

  return new Response(upstream.body, {
    status: upstream.status,
    headers,
  });
}

async function handleRequest(request, env) {
  if (request.method === "OPTIONS") {
    return new Response(null, {
      status: 204,
      headers: RESPONSE_HEADERS,
    });
  }

  const url = new URL(request.url);
  if (url.pathname === "/img") {
    return handlePublicImageProxy(request);
  }

  return handlePrivateProxy(request, env);
}

export default {
  async fetch(request, env) {
    try {
      return await handleRequest(request, env);
    } catch (error) {
      const message =
        error && error.message ? error.message : String(error);
      return text(500, `worker error: ${message}`);
    }
  },
};
