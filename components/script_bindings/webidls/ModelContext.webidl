/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// WebMCP, Community Group draft of 27 October 2026.
// https://webmachinelearning.github.io/webmcp/
//
// Schema and arguments are JSON text. The draft's object form is the same
// data; a DOMString keeps the binding off JS object conversion. The form
// attributes (`toolname`, `toolautosubmit`) are not part of this interface.

callback WebMcpExecuteCallback = DOMString (DOMString input);

[Exposed=Window]
interface ModelContext : EventTarget {
  undefined registerTool(DOMString name, DOMString description, DOMString inputSchema, WebMcpExecuteCallback execute);
  undefined unregisterTool(DOMString name);
  sequence<DOMString> getTools();
  Promise<DOMString> executeTool(DOMString name, DOMString arguments);
  undefined cancel(DOMString invocationId);
  attribute EventHandler ontoolschanged;
};
