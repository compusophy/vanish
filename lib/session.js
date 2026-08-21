import crypto from 'crypto';

// stateless encrypted session cookie. no database required.
// payload is sealed with aes-256-gcm so the embedded github token
// stays unreadable even if the cookie is captured off the wire.

const COOKIE_NAME = 'vanish_session';
const STATE_COOKIE = 'vanish_oauth_state';
const MAX_AGE_SECONDS = 60 * 60 * 24 * 7; // 7 days

function getKey() {
  const secret = process.env.SESSION_SECRET;
  if (!secret) {
    throw new Error('SESSION_SECRET is not configured');
  }
  // normalize any length secret into a 32 byte key
  return crypto.createHash('sha256').update(secret).digest();
}

export function seal(payload) {
  const key = getKey();
  const iv = crypto.randomBytes(12);
  const cipher = crypto.createCipheriv('aes-256-gcm', key, iv);
  const json = JSON.stringify({ ...payload, iat: Date.now() });
  const encrypted = Buffer.concat([cipher.update(json, 'utf8'), cipher.final()]);
  const tag = cipher.getAuthTag();
  return [iv, tag, encrypted].map((b) => b.toString('base64url')).join('.');
}

export function unseal(token) {
  try {
    const key = getKey();
    const [ivB64, tagB64, dataB64] = String(token).split('.');
    if (!ivB64 || !tagB64 || !dataB64) return null;

    const decipher = crypto.createDecipheriv(
      'aes-256-gcm',
      key,
      Buffer.from(ivB64, 'base64url')
    );
    decipher.setAuthTag(Buffer.from(tagB64, 'base64url'));
    const decrypted = Buffer.concat([
      decipher.update(Buffer.from(dataB64, 'base64url')),
      decipher.final()
    ]).toString('utf8');

    const payload = JSON.parse(decrypted);
    if (!payload.iat || Date.now() - payload.iat > MAX_AGE_SECONDS * 1000) {
      return null;
    }
    return payload;
  } catch (err) {
    // tampered, expired key rotation, or malformed cookie
    return null;
  }
}

// minimal cookie header parser (avoids pulling in cookie-parser)
export function parseCookies(req) {
  const header = req.headers?.cookie;
  if (!header) return {};
  return header.split(';').reduce((acc, part) => {
    const idx = part.indexOf('=');
    if (idx === -1) return acc;
    const key = part.slice(0, idx).trim();
    const value = part.slice(idx + 1).trim();
    if (key) acc[key] = decodeURIComponent(value);
    return acc;
  }, {});
}

function serializeCookie(name, value, { maxAge, secure }) {
  const parts = [
    `${name}=${encodeURIComponent(value)}`,
    'Path=/',
    'HttpOnly',
    'SameSite=Lax',
    `Max-Age=${maxAge}`
  ];
  if (secure) parts.push('Secure');
  return parts.join('; ');
}

function isSecure(req) {
  return (req.headers['x-forwarded-proto'] || req.protocol) === 'https';
}

export function setSessionCookie(req, res, payload) {
  res.append(
    'Set-Cookie',
    serializeCookie(COOKIE_NAME, seal(payload), {
      maxAge: MAX_AGE_SECONDS,
      secure: isSecure(req)
    })
  );
}

export function clearSessionCookie(req, res) {
  res.append(
    'Set-Cookie',
    serializeCookie(COOKIE_NAME, '', { maxAge: 0, secure: isSecure(req) })
  );
}

export function readSession(req) {
  const cookies = parseCookies(req);
  if (!cookies[COOKIE_NAME]) return null;
  return unseal(cookies[COOKIE_NAME]);
}

export function setStateCookie(req, res, state) {
  res.append(
    'Set-Cookie',
    serializeCookie(STATE_COOKIE, state, { maxAge: 600, secure: isSecure(req) })
  );
}

export function readStateCookie(req) {
  return parseCookies(req)[STATE_COOKIE] || null;
}

export function clearStateCookie(req, res) {
  res.append(
    'Set-Cookie',
    serializeCookie(STATE_COOKIE, '', { maxAge: 0, secure: isSecure(req) })
  );
}
