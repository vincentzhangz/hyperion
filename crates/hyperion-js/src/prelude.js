// Hyperion DOM/Window prelude. Installed once per isolate after __hyperion_op
// and console. DOM ops are multiplexed on op id 1.
(function () {
  var DOM_OP = 1;

  function call(method, args) {
    var payload = args || {};
    payload.method = method;
    var raw = __hyperion_op(DOM_OP, JSON.stringify(payload));
    var resp;
    try {
      resp = JSON.parse(raw);
    } catch (e) {
      throw new Error("dom op parse error: " + raw);
    }
    if (resp && resp.error) {
      throw new Error(resp.error);
    }
    return resp;
  }

  var nodeCache = Object.create(null);

  function wrap(id) {
    if (id === null || id === undefined) return null;
    if (nodeCache[id]) return nodeCache[id];
    var n = new Node(id);
    nodeCache[id] = n;
    return n;
  }

  function Node(id) {
    Object.defineProperty(this, "__nodeId", { value: id, enumerable: false });
  }

  Node.prototype.toString = function () {
    return "[object Node]";
  };

  Object.defineProperty(Node.prototype, "nodeType", {
    get: function () {
      return call("nodeType", { nodeId: this.__nodeId }).value;
    },
  });
  Object.defineProperty(Node.prototype, "nodeName", {
    get: function () {
      return call("nodeName", { nodeId: this.__nodeId }).value;
    },
  });
  Object.defineProperty(Node.prototype, "localName", {
    get: function () {
      return call("localName", { nodeId: this.__nodeId }).value;
    },
  });
  Object.defineProperty(Node.prototype, "nodeValue", {
    get: function () {
      return call("nodeValue", { nodeId: this.__nodeId }).value;
    },
    set: function (v) {
      call("setNodeValue", { nodeId: this.__nodeId, value: String(v) });
    },
  });
  Object.defineProperty(Node.prototype, "textContent", {
    get: function () {
      return call("textContent", { nodeId: this.__nodeId }).value;
    },
    set: function (v) {
      call("setTextContent", { nodeId: this.__nodeId, value: String(v) });
    },
  });
  Object.defineProperty(Node.prototype, "parentNode", {
    get: function () {
      return wrap(call("parentNode", { nodeId: this.__nodeId }).nodeId);
    },
  });
  Object.defineProperty(Node.prototype, "parentElement", {
    get: function () {
      return wrap(call("parentElement", { nodeId: this.__nodeId }).nodeId);
    },
  });
  Object.defineProperty(Node.prototype, "childNodes", {
    get: function () {
      var ids = call("childNodes", { nodeId: this.__nodeId }).nodeIds || [];
      return ids.map(wrap);
    },
  });
  Object.defineProperty(Node.prototype, "firstChild", {
    get: function () {
      var kids = this.childNodes;
      return kids.length ? kids[0] : null;
    },
  });
  Object.defineProperty(Node.prototype, "lastChild", {
    get: function () {
      var kids = this.childNodes;
      return kids.length ? kids[kids.length - 1] : null;
    },
  });
  Object.defineProperty(Node.prototype, "children", {
    get: function () {
      return this.childNodes.filter(function (n) {
        return n.nodeType === 1;
      });
    },
  });
  Object.defineProperty(Node.prototype, "innerHTML", {
    get: function () {
      return call("innerHTML", { nodeId: this.__nodeId }).value;
    },
    set: function (v) {
      call("setInnerHTML", { nodeId: this.__nodeId, html: String(v) });
      nodeCache = Object.create(null);
    },
  });
  Object.defineProperty(Node.prototype, "outerHTML", {
    get: function () {
      return call("outerHTML", { nodeId: this.__nodeId }).value;
    },
    set: function (v) {
      call("setOuterHTML", { nodeId: this.__nodeId, html: String(v) });
      nodeCache = Object.create(null);
    },
  });

  Node.prototype.getAttribute = function (name) {
    var r = call("getAttribute", { nodeId: this.__nodeId, name: String(name) });
    return r.value === undefined ? null : r.value;
  };
  Node.prototype.setAttribute = function (name, value) {
    call("setAttribute", {
      nodeId: this.__nodeId,
      name: String(name),
      value: String(value),
    });
  };
  Node.prototype.removeAttribute = function (name) {
    call("removeAttribute", { nodeId: this.__nodeId, name: String(name) });
  };
  Node.prototype.hasAttribute = function (name) {
    return this.getAttribute(name) !== null;
  };
  Node.prototype.querySelector = function (sel) {
    return wrap(
      call("querySelector", { nodeId: this.__nodeId, selector: String(sel) })
        .nodeId
    );
  };
  Node.prototype.querySelectorAll = function (sel) {
    var ids =
      call("querySelectorAll", {
        nodeId: this.__nodeId,
        selector: String(sel),
      }).nodeIds || [];
    return ids.map(wrap);
  };
  Node.prototype.appendChild = function (child) {
    if (!child || child.__nodeId === undefined) {
      throw new TypeError("appendChild expects a Node");
    }
    call("appendChild", {
      parentId: this.__nodeId,
      childId: child.__nodeId,
    });
    return child;
  };
  Node.prototype.removeChild = function (child) {
    if (!child || child.__nodeId === undefined) {
      throw new TypeError("removeChild expects a Node");
    }
    call("removeChild", {
      parentId: this.__nodeId,
      childId: child.__nodeId,
    });
    return child;
  };
  Node.prototype.remove = function () {
    call("removeNode", { nodeId: this.__nodeId });
  };
  Node.prototype.focus = function () {
    call("focus", { nodeId: this.__nodeId });
  };
  Node.prototype.click = function () {
    call("dispatchEvent", {
      nodeId: this.__nodeId,
      type: "click",
      bubbles: true,
    });
  };
  Node.prototype.addEventListener = function (type, listener) {
    if (typeof listener !== "function") return;
    if (!this.__listeners) this.__listeners = Object.create(null);
    var list = this.__listeners[type] || (this.__listeners[type] = []);
    list.push(listener);
    call("addEventListener", {
      nodeId: this.__nodeId,
      type: String(type),
    });
  };
  Node.prototype.removeEventListener = function (type, listener) {
    if (!this.__listeners || !this.__listeners[type]) return;
    this.__listeners[type] = this.__listeners[type].filter(function (fn) {
      return fn !== listener;
    });
  };
  Node.prototype.dispatchEvent = function (event) {
    var type = typeof event === "string" ? event : event && event.type;
    if (!type) return false;
    call("dispatchEvent", {
      nodeId: this.__nodeId,
      type: String(type),
      bubbles: !!(event && event.bubbles),
    });
    var list = this.__listeners && this.__listeners[type];
    if (list) {
      for (var i = 0; i < list.length; i++) {
        try {
          list[i].call(this, event || { type: type });
        } catch (e) {}
      }
    }
    return true;
  };

  Object.defineProperty(Node.prototype, "id", {
    get: function () {
      return this.getAttribute("id") || "";
    },
    set: function (v) {
      this.setAttribute("id", v);
    },
  });
  Object.defineProperty(Node.prototype, "className", {
    get: function () {
      return this.getAttribute("class") || "";
    },
    set: function (v) {
      this.setAttribute("class", v);
    },
  });
  Object.defineProperty(Node.prototype, "value", {
    get: function () {
      return this.getAttribute("value") || "";
    },
    set: function (v) {
      this.setAttribute("value", v);
    },
  });
  Object.defineProperty(Node.prototype, "href", {
    get: function () {
      return this.getAttribute("href") || "";
    },
    set: function (v) {
      this.setAttribute("href", v);
    },
  });
  Object.defineProperty(Node.prototype, "src", {
    get: function () {
      return this.getAttribute("src") || "";
    },
    set: function (v) {
      this.setAttribute("src", v);
    },
  });
  Object.defineProperty(Node.prototype, "tagName", {
    get: function () {
      return this.nodeName;
    },
  });

  function Document() {
    Node.call(this, call("documentNode", {}).nodeId);
  }
  Document.prototype = Object.create(Node.prototype);
  Document.prototype.constructor = Document;
  Document.prototype.createElement = function (tag) {
    return wrap(call("createElement", { tag: String(tag) }).nodeId);
  };
  Document.prototype.createTextNode = function (text) {
    return wrap(call("createTextNode", { text: String(text) }).nodeId);
  };
  Document.prototype.getElementById = function (id) {
    return this.querySelector("#" + CSS.escape(String(id)));
  };
  Document.prototype.getElementsByTagName = function (tag) {
    return this.querySelectorAll(String(tag));
  };
  Document.prototype.getElementsByClassName = function (cls) {
    return this.querySelectorAll("." + String(cls).split(/\s+/).join("."));
  };
  Object.defineProperty(Document.prototype, "documentElement", {
    get: function () {
      return this.querySelector("html");
    },
  });
  Object.defineProperty(Document.prototype, "head", {
    get: function () {
      return this.querySelector("head");
    },
  });
  Object.defineProperty(Document.prototype, "body", {
    get: function () {
      return this.querySelector("body");
    },
  });
  Object.defineProperty(Document.prototype, "title", {
    get: function () {
      var t = this.querySelector("title");
      return t ? t.textContent : "";
    },
    set: function (v) {
      var t = this.querySelector("title");
      if (!t) {
        var head = this.head;
        if (head) {
          t = this.createElement("title");
          head.appendChild(t);
        }
      }
      if (t) t.textContent = String(v);
    },
  });
  Object.defineProperty(Document.prototype, "URL", {
    get: function () {
      return call("location", {}).href || "";
    },
  });
  Object.defineProperty(Document.prototype, "documentURI", {
    get: function () {
      return this.URL;
    },
  });

  // Minimal CSS.escape for getElementById
  var CSS = {
    escape: function (s) {
      return String(s).replace(/[^a-zA-Z0-9_-]/g, function (c) {
        return "\\" + c;
      });
    },
  };

  var locationInfo = call("location", {});
  var locationObj = {
    href: locationInfo.href || "",
    origin: locationInfo.origin || "",
    protocol: locationInfo.protocol || "",
    host: locationInfo.host || "",
    hostname: locationInfo.hostname || "",
    port: locationInfo.port || "",
    pathname: locationInfo.pathname || "",
    search: locationInfo.search || "",
    hash: locationInfo.hash || "",
    toString: function () {
      return this.href;
    },
  };

  var viewport = call("viewport", {});
  var windowObj = globalThis;
  windowObj.window = windowObj;
  windowObj.self = windowObj;
  windowObj.globalThis = windowObj;
  windowObj.document = new Document();
  windowObj.location = locationObj;
  windowObj.Node = Node;
  windowObj.Document = Document;
  windowObj.Element = Node;
  windowObj.HTMLElement = Node;
  windowObj.CSS = CSS;
  windowObj.innerWidth = viewport.width || 980;
  windowObj.innerHeight = viewport.height || 1024;
  windowObj.outerWidth = windowObj.innerWidth;
  windowObj.outerHeight = windowObj.innerHeight;
  windowObj.devicePixelRatio = viewport.dpr || 1;
  windowObj.navigator = {
    userAgent: locationInfo.userAgent || "hyperion/0.1",
    language: "en-US",
    platform: "Linux",
  };
  windowObj.getComputedStyle = function () {
    return new Proxy(
      {},
      {
        get: function () {
          return "";
        },
      }
    );
  };
  windowObj.setTimeout = function (fn) {
    if (typeof fn === "function") {
      try {
        fn();
      } catch (e) {}
    }
    return 0;
  };
  windowObj.clearTimeout = function () {};
  windowObj.setInterval = function () {
    return 0;
  };
  windowObj.clearInterval = function () {};
  windowObj.requestAnimationFrame = function (fn) {
    if (typeof fn === "function") {
      try {
        fn(0);
      } catch (e) {}
    }
    return 0;
  };
  windowObj.cancelAnimationFrame = function () {};
  windowObj.addEventListener = function () {};
  windowObj.removeEventListener = function () {};
  windowObj.dispatchEvent = function () {
    return true;
  };
  windowObj.focus = function () {};
  windowObj.blur = function () {};
  windowObj.scrollTo = function () {};
  windowObj.scrollBy = function () {};
  function originOf(href) {
    try {
      var u = new URL(href);
      return u.origin;
    } catch (e) {
      return href || "null";
    }
  }

  windowObj.__hyperion_wrapNode = wrap;
  windowObj.__hyperion_invalidateNodeCache = function () {
    nodeCache = Object.create(null);
  };

  function StorageArea(kind) {
    this._kind = kind;
  }
  StorageArea.prototype.getItem = function (key) {
    var r = call("storageGet", { kind: this._kind, key: String(key) });
    return r.value == null ? null : r.value;
  };
  StorageArea.prototype.setItem = function (key, value) {
    call("storageSet", { kind: this._kind, key: String(key), value: String(value) });
  };
  StorageArea.prototype.removeItem = function (key) {
    call("storageRemove", { kind: this._kind, key: String(key) });
  };
  StorageArea.prototype.clear = function () {
    call("storageClear", { kind: this._kind });
  };
  StorageArea.prototype.key = function (i) {
    var r = call("storageKey", { kind: this._kind, index: i | 0 });
    return r.value == null ? null : r.value;
  };
  Object.defineProperty(StorageArea.prototype, "length", {
    get: function () {
      return call("storageLength", { kind: this._kind }).value || 0;
    },
  });

  windowObj.localStorage = new StorageArea("local");
  windowObj.sessionStorage = new StorageArea("session");

  "ok";
})();
