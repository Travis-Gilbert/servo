/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// WebMCP, Community Group draft of 27 October 2026.
// https://webmachinelearning.github.io/webmcp/
//
// Registration takes the draft's tool dictionary. `execute` returns a
// promise. JSON stays a string only on the internal inspection channel.
// Form attributes (`toolname`, `toolautosubmit`) are not part of this interface.

callback ToolExecuteCallback = Promise<any> (object input);

dictionary ModelContextTool {
  required DOMString name;
  required DOMString description;
  required object inputSchema;
  required ToolExecuteCallback execute;
};

dictionary RegisteredTool {
  required DOMString name;
  DOMString description;
  object inputSchema;
};

[Exposed=Window, SecureContext, Pref="dom_webmcp_enabled"]
interface ModelContext : EventTarget {
  Promise<undefined> registerTool(ModelContextTool tool);
  Promise<sequence<RegisteredTool>> getTools();
  Promise<any> executeTool(DOMString name, optional object input);
  undefined unregisterTool(DOMString name);
  attribute EventHandler ontoolchange;
};
