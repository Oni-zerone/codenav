export interface Client { get(path: string): Promise<string>; }

export class HttpClient implements Client {
  async get(path: string): Promise<string> { return fetchJson(path); }
}

export function fetchJson(path: string): Promise<string> {
  return fetch(path).then(r => r.text());
}
