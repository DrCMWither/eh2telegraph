/*
    Cloudflare workers telegraph proxy.
    Deploy and set `KEY` variable in browser.
*/

const RESPONSE_HEADERS = {
    Server: "web-proxy",
    "Access-Control-Allow-Origin": "*",
    "Access-Control-Allow-Methods": "GET, POST, HEAD, OPTIONS",
    "Access-Control-Allow-Headers": "X-Authorization, X-Target-URL, Content-Type, Accept",
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
]);

const IMAGE_ALLOWED_HOSTS = new Set([
    "i.nhentai.net",
    "i2.nhentai.net",
    "i3.nhentai.net",
    "t.nhentai.net",

    "ehgt.org",
    "e-hentai.org",
    "exhentai.org",
]);

const MAX_IMAGE_BYTES = 50 * 1024 * 1024;

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
    const host = hostname.toLowerCase();
    return PRIVATE_ALLOWED_HOSTS.has(host);
}

function isAllowedImageHost(hostname) {
    const host = hostname.toLowerCase();

    if (IMAGE_ALLOWED_HOSTS.has(host)) {
        return true;
    }

    if (host.endsWith(".hath.network")) {
        return true;
    }

    return false;
}

function buildPrivateHeaders(request) {
    const out = new Headers();

    for (const name of ["Content-Type", "Accept", "User-Agent"]) {
        const value = request.headers.get(name);
        if (value) {
            out.set(name, value);
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

    for (const [k, v] of Object.entries(extra)) {
        headers.set(k, v);
    }

    return headers;
}

async function handlePrivateProxy(request, env) {
    if (request.headers.get("X-Authorization") !== env.KEY) {
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

    const init = {
        method: request.method,
        headers: buildPrivateHeaders(request),
    };

    if (request.method !== "GET" && request.method !== "HEAD") {
        init.body = request.body;
    }

    const upstream = await fetch(targetUrl.toString(), init);
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
        } catch (e) {
            return text(500, `worker error: ${e && e.message ? e.message : String(e)}`);
        }
    },
};