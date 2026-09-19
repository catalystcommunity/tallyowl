// Signing in, and the callback route that finishes it.
//
// `docs/DECISIONS.md` D7: LinkKeys owns human authentication and TallyOwl owns
// the resulting session and authorization. This module never sees a password
// and never verifies an assertion; it begins a login, sends the browser where
// the head said, and hands back what the callback carried.
//
// The session token is kept in `sessionStorage` rather than `localStorage`:
// it ends with the tab. A token that outlived the browser session would sit in
// a shared machine's profile for whoever opens it next.

import {
  fromBeginLoginResponseCbor,
  fromCompleteLoginResponseCbor,
  toBeginLoginRequestCbor,
  toCompleteLoginRequestCbor,
  type CompleteLoginResponse,
} from "./control-api.ts";
import { type Control } from "./transport.ts";

const SESSION_KEY = "tallyowl.session";
const PENDING_KEY = "tallyowl.login";

export interface Storage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/// The session this tab holds, if any.
export function heldSession(storage: Storage): string | undefined {
  return storage.getItem(SESSION_KEY) ?? undefined;
}

export function forgetSession(storage: Storage): void {
  storage.removeItem(SESSION_KEY);
  storage.removeItem(PENDING_KEY);
}

/// Begin a sign-in and return where to send the browser.
///
/// The callback URL is the installation's configured one, and the head refuses
/// any other. A caller that could choose it could have the token delivered
/// somewhere it picked.
export async function begin(
  control: Control,
  storage: Storage,
  userDomain: string,
  callbackUrl: string,
): Promise<string> {
  const payload = await control.call(
    "begin-login",
    toBeginLoginRequestCbor({ userDomain, callbackUrl }),
  );
  const response = fromBeginLoginResponseCbor(payload);
  // The login ID names the pending login and is not a credential: completing
  // it still needs what the callback carried.
  storage.setItem(PENDING_KEY, response.loginId);
  return response.redirectUrl;
}

/// Finish a sign-in from the URL the callback arrived at.
///
/// Returns nothing when this is not a callback, so the entry point can call it
/// on every load without deciding first.
export async function complete(
  control: Control,
  storage: Storage,
  arrivedUrl: string,
): Promise<CompleteLoginResponse | undefined> {
  const parsed = new URL(arrivedUrl);
  const encryptedToken = parsed.searchParams.get("encrypted_token");
  const loginId = storage.getItem(PENDING_KEY);
  if (encryptedToken === null || loginId === null) return undefined;

  // Taken rather than read. A completion that fails leaves nothing to try
  // again with a different token, which is the replay protection the SDK says
  // is the application's job.
  storage.removeItem(PENDING_KEY);

  const payload = await control.call(
    "complete-login",
    toCompleteLoginRequestCbor({ loginId, encryptedToken, arrivedUrl }),
  );
  const response = fromCompleteLoginResponseCbor(payload);
  storage.setItem(SESSION_KEY, response.sessionToken);
  control.withSession(response.sessionToken);
  return response;
}
