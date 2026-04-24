/*
    Cloudflare workers telegraph proxy.
    Deploy and set `KEY` variable in browser.
*/

addEventListener("fetch", event => {
  event.respondWith(handleRequest(event.request));
});

const RESPONSE_HEADERS = {
  "Server": "web-proxy",
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
  "Access-Control-Allow-Headers": "*",
};

function text(status, body) {
  return new Response(body, {
    status,
    headers: {
      ...RESPONSE_HEADERS,
      "Content-Type": "text/plain; charset=utf-8",
    },
  });
}

function isAllowedImageHost(hostname) {
  const allowed = new Set([
    "i.nhentai.net",
    "i2.nhentai.net",
    "i3.nhentai.net",

    // Preserved
    "ehgt.org",
    "e-hentai.org",
    "exhentai.org",
  ]);
  return allowed.has(hostname);
}

async function handlePrivateProxy(request) {
  if (request.headers.get("X-Authorization") !== KEY) {
    return text(401, "unauthorized");
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

  let proxiedRequest;
  if (request.body && request.method !== "GET" && request.method !== "HEAD") {
    proxiedRequest = new Request(targetUrl, {
      method: request.method,
      headers: request.headers,
      body: request.body,
    });
  } else {
    proxiedRequest = new Request(targetUrl, {
      method: request.method,
      headers: request.headers,
    });
  }

  proxiedRequest.headers.delete("X-Authorization");
  proxiedRequest.headers.delete("X-Target-URL");
  proxiedRequest.headers.delete("CF-Connecting-IP");
  proxiedRequest.headers.delete("CF-Worker");
  proxiedRequest.headers.delete("CF-EW-Via");

  return fetch(proxiedRequest);
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

  if (!isAllowedImageHost(targetUrl.hostname)) {
    return text(403, "forbidden host");
  }

  const headers = new Headers();
  headers.set(
    "User-Agent",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36"
  );

  // Bring Referer to nhentai image
  if (targetUrl.hostname.endsWith("nhentai.net")) {
    headers.set("Referer", "https://nhentai.net/");
  }

  const upstream = await fetch(targetUrl.toString(), {
    method: request.method,
    headers,
    cf: {
      cacheTtl: 86400,
      cacheEverything: true,
    },
  });

  if (!upstream.ok) {
    return text(upstream.status, `upstream ${upstream.status}`);
  }

  const outHeaders = new Headers(upstream.headers);

  outHeaders.set("Cache-Control", "public, max-age=86400");
  outHeaders.set("Access-Control-Allow-Origin", "*");

  return new Response(upstream.body, {
    status: upstream.status,
    headers: outHeaders,
  });
}

async function handleRequest(request) {
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

  return handlePrivateProxy(request);
}