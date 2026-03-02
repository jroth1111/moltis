(function () {
  'use strict';

  // === MOLTIS STEALTH CONFIG (values replaced at runtime by build_evasion_script) ===
  const STEALTH_HARDWARE_CONCURRENCY = 4;
  const STEALTH_DEVICE_MEMORY = 8;
  const STEALTH_WEBGL_VENDOR = 'Intel Inc.';
  const STEALTH_WEBGL_RENDERER = 'Intel Iris OpenGL Engine';
  const STEALTH_LANGUAGES = ['en-US', 'en'];
  // ==================================================================================

  // Stealth utility functions
  // Based on: https://github.com/berstend/puppeteer-extra/tree/master/packages/puppeteer-extra-plugin-stealth
  const utils = {
    // Generate a source-URL-free representation of a code string
    stripSourceUrl: (str) => str.replace(/\s*\/\/[@#]\s*sourceURL=.+/g, ''),

    // Make a function appear native
    makeNativeString: (name = '') => {
      return name ? `function ${name}() { [native code] }` : 'function () { [native code] }';
    },

    // Redefine a function's toString to appear native
    redefineToString: (fn, name) => {
      const nativeString = utils.makeNativeString(name);
      const originalToString = Function.prototype.toString;
      const handler = {
        apply: function (target, thisArg, args) {
          if (thisArg === fn) {
            return nativeString;
          }
          return originalToString.apply(thisArg, args);
        },
      };
      Function.prototype.toString = new Proxy(originalToString, handler);
    },

    // Create a Proxy handler that returns native-looking toString
    createProxyHandler: (nativeString) => ({
      get: (target, prop) => {
        if (prop === 'toString') {
          return () => nativeString;
        }
        return Reflect.get(target, prop);
      },
      apply: (target, thisArg, args) => {
        return Reflect.apply(target, thisArg, args);
      },
    }),

    // Replace a getter with a proxy
    replaceGetterWithProxy: (obj, prop, handler) => {
      const descriptor = Object.getOwnPropertyDescriptor(obj, prop);
      if (descriptor && descriptor.get) {
        const originalGetter = descriptor.get;
        const newGetter = new Proxy(originalGetter, handler);
        Object.defineProperty(obj, prop, {
          ...descriptor,
          get: newGetter,
        });
      }
    },
  };

  // ============================================================
  // 1. navigator.webdriver (CRITICAL - most common detection)
  // ============================================================
  try {
    const proto = Object.getPrototypeOf(navigator);
    const protoDesc = proto && Object.getOwnPropertyDescriptor(proto, 'webdriver');
    if (protoDesc && protoDesc.configurable) {
      delete proto.webdriver;
    }

    const navDesc = Object.getOwnPropertyDescriptor(navigator, 'webdriver');
    if (navDesc && navDesc.configurable) {
      delete navigator.webdriver;
    }

    if ('webdriver' in navigator) {
      if (proto && (!protoDesc || protoDesc.configurable)) {
        try {
          Object.defineProperty(proto, 'webdriver', {
            get: () => undefined,
            configurable: true,
            enumerable: false,
          });
        } catch (e) {}
      }
      try {
        Object.defineProperty(navigator, 'webdriver', {
          get: () => undefined,
          configurable: true,
          enumerable: false,
        });
      } catch (e) {}
    }
  } catch (e) {}

  // ============================================================
  // 2. chrome.runtime (for extension detection bypass)
  // ============================================================
  try {
    if (!window.chrome) {
      window.chrome = {};
    }

    if (!window.chrome.runtime) {
      window.chrome.runtime = {
        connect: function (extensionId, connectInfo) {
          throw new Error(
            'Could not establish connection. Receiving end does not exist.'
          );
        },
        sendMessage: function (extensionId, message, options, responseCallback) {
          if (typeof options === 'function') {
            responseCallback = options;
            options = {};
          }
          if (responseCallback) {
            responseCallback(undefined);
          }
          return undefined;
        },
        id: undefined,
        getManifest: function () {
          return undefined;
        },
        getURL: function (path) {
          return '';
        },
        getPlatformInfo: function (callback) {
          callback({ os: 'mac', arch: 'x86-64', nacl_arch: 'x86-64' });
        },
        OnInstalledReason: {
          CHROME_UPDATE: 'chrome_update',
          INSTALL: 'install',
          SHARED_MODULE_UPDATE: 'shared_module_update',
          UPDATE: 'update',
        },
      };
    }
  } catch (e) {}

  // ============================================================
  // 3. chrome.app (headless detection bypass)
  // ============================================================
  try {
    if (!window.chrome) {
      window.chrome = {};
    }

    if (!window.chrome.app) {
      window.chrome.app = {
        InstallState: {
          DISABLED: 'disabled',
          INSTALLED: 'installed',
          NOT_INSTALLED: 'not_installed',
        },
        RunningState: {
          CANNOT_RUN: 'cannot_run',
          READY_TO_RUN: 'ready_to_run',
          RUNNING: 'running',
        },
        getDetails: function () {
          return null;
        },
        getIsInstalled: function () {
          return false;
        },
        installState: function (callback) {
          callback('not_installed');
        },
        isInstalled: false,
        runningState: function () {
          return 'cannot_run';
        },
      };
    }
  } catch (e) {}

  // ============================================================
  // 4. chrome.csi (deprecated but checked)
  // ============================================================
  try {
    if (!window.chrome) {
      window.chrome = {};
    }

    if (!window.chrome.csi) {
      window.chrome.csi = function () {
        return {
          startE: Date.now(),
          onloadT: Date.now(),
          pageT: Math.random() * 1000 + 1000,
          tran: 15,
        };
      };
    }
  } catch (e) {}

  // ============================================================
  // 5. chrome.loadTimes (deprecated but checked)
  // ============================================================
  try {
    if (!window.chrome) {
      window.chrome = {};
    }

    if (!window.chrome.loadTimes) {
      window.chrome.loadTimes = function () {
        const timing = window.performance.timing;
        const navStart = timing.navigationStart;
        return {
          commitLoadTime: (timing.responseStart || navStart) / 1000,
          connectionInfo: 'h2',
          finishDocumentLoadTime:
            (timing.domContentLoadedEventEnd || navStart) / 1000,
          finishLoadTime: (timing.loadEventEnd || navStart) / 1000,
          firstPaintAfterLoadTime: 0,
          firstPaintTime:
            (timing.domContentLoadedEventStart || navStart) / 1000,
          navigationType: 'Other',
          npnNegotiatedProtocol: 'h2',
          requestTime: (timing.requestStart || navStart) / 1000,
          startLoadTime: navStart / 1000,
          wasAlternateProtocolAvailable: false,
          wasFetchedViaSpdy: true,
          wasNpnNegotiated: true,
        };
      };
    }
  } catch (e) {}

  // ============================================================
  // 6. navigator.plugins + navigator.mimeTypes (magic proxy arrays)
  // ============================================================
  try {
    const hasPlugins =
      'plugins' in navigator && navigator.plugins && navigator.plugins.length;
    const hasConstructors =
      typeof PluginArray === 'function' &&
      typeof Plugin === 'function' &&
      typeof MimeTypeArray === 'function' &&
      typeof MimeType === 'function';

    if (!hasPlugins && hasConstructors) {
      const data = {
        mimeTypes: [
          {
            type: 'application/pdf',
            suffixes: 'pdf',
            description: '',
            __pluginName: 'Chrome PDF Viewer',
          },
          {
            type: 'application/x-google-chrome-pdf',
            suffixes: 'pdf',
            description: 'Portable Document Format',
            __pluginName: 'Chrome PDF Plugin',
          },
          {
            type: 'application/x-nacl',
            suffixes: '',
            description: 'Native Client Executable',
            __pluginName: 'Native Client',
          },
          {
            type: 'application/x-pnacl',
            suffixes: '',
            description: 'Portable Native Client Executable',
            __pluginName: 'Native Client',
          },
        ],
        plugins: [
          {
            name: 'Chrome PDF Plugin',
            filename: 'internal-pdf-viewer',
            description: 'Portable Document Format',
            __mimeTypes: ['application/x-google-chrome-pdf'],
          },
          {
            name: 'Chrome PDF Viewer',
            filename: 'mhjfbmdgcfjbbpaeojofohoefgiehjai',
            description: '',
            __mimeTypes: ['application/pdf'],
          },
          {
            name: 'Native Client',
            filename: 'internal-nacl-plugin',
            description: '',
            __mimeTypes: ['application/x-nacl', 'application/x-pnacl'],
          },
        ],
      };

      const generateFunctionMocks = (proto, itemMainProp, dataArray) => ({
        item: new Proxy(proto.item, {
          apply(target, ctx, args) {
            if (!args.length) {
              throw new TypeError(
                "Failed to execute 'item' on '" +
                  proto[Symbol.toStringTag] +
                  "': 1 argument required, but only 0 present."
              );
            }
            const isInteger = args[0] && Number.isInteger(Number(args[0]));
            return (isInteger ? dataArray[Number(args[0])] : dataArray[0]) || null;
          },
        }),
        namedItem: new Proxy(proto.namedItem, {
          apply(target, ctx, args) {
            if (!args.length) {
              throw new TypeError(
                "Failed to execute 'namedItem' on '" +
                  proto[Symbol.toStringTag] +
                  "': 1 argument required, but only 0 present."
              );
            }
            return dataArray.find((mt) => mt[itemMainProp] === args[0]) || null;
          },
        }),
        refresh: proto.refresh
          ? new Proxy(proto.refresh, {
              apply() {
                return undefined;
              },
            })
          : undefined,
      });

      const generateMagicArray = (dataArray, proto, itemProto, itemMainProp) => {
        const defineProp = (obj, prop, value) =>
          Object.defineProperty(obj, prop, {
            value,
            writable: false,
            enumerable: false,
            configurable: true,
          });

        const patchItem = (item, data) => {
          let descriptor = Object.getOwnPropertyDescriptors(item);

          if (itemProto === Plugin.prototype) {
            descriptor = {
              ...descriptor,
              length: {
                value: data.__mimeTypes.length,
                writable: false,
                enumerable: false,
                configurable: true,
              },
            };
          }

          const obj = Object.create(itemProto, descriptor);

          const blacklist = [...Object.keys(data), 'length', 'enabledPlugin'];
          return new Proxy(obj, {
            ownKeys(target) {
              return Reflect.ownKeys(target).filter(
                (k) => !blacklist.includes(k)
              );
            },
            getOwnPropertyDescriptor(target, prop) {
              if (blacklist.includes(prop)) {
                return undefined;
              }
              return Reflect.getOwnPropertyDescriptor(target, prop);
            },
          });
        };

        const makeItem = (data) => {
          const item = {};
          for (const prop of Object.keys(data)) {
            if (prop.startsWith('__')) {
              continue;
            }
            defineProp(item, prop, data[prop]);
          }
          return patchItem(item, data);
        };

        const magicArray = [];
        dataArray.forEach((dataEntry) => {
          magicArray.push(makeItem(dataEntry));
        });

        magicArray.forEach((entry) => {
          defineProp(magicArray, entry[itemMainProp], entry);
        });

        const magicArrayObj = Object.create(proto, {
          ...Object.getOwnPropertyDescriptors(magicArray),
          length: {
            value: magicArray.length,
            writable: false,
            enumerable: false,
            configurable: true,
          },
        });

        const functionMocks = generateFunctionMocks(proto, itemMainProp, magicArray);

        return new Proxy(magicArrayObj, {
          get(target, key) {
            if (key === 'item') {
              return functionMocks.item;
            }
            if (key === 'namedItem') {
              return functionMocks.namedItem;
            }
            if (proto === PluginArray.prototype && key === 'refresh') {
              return functionMocks.refresh;
            }
            return Reflect.get(target, key);
          },
          ownKeys(target) {
            const keys = [];
            const typeProps = magicArray.map((mt) => mt[itemMainProp]);
            typeProps.forEach((_, i) => keys.push(String(i)));
            typeProps.forEach((propName) => keys.push(propName));
            return keys;
          },
          getOwnPropertyDescriptor(target, prop) {
            if (prop === 'length') {
              return undefined;
            }
            return Reflect.getOwnPropertyDescriptor(target, prop);
          },
        });
      };

      const generateMimeTypeArray = (mimeTypesData) =>
        generateMagicArray(
          mimeTypesData,
          MimeTypeArray.prototype,
          MimeType.prototype,
          'type'
        );

      const generatePluginArray = (pluginsData) =>
        generateMagicArray(pluginsData, PluginArray.prototype, Plugin.prototype, 'name');

      const mimeTypes = generateMimeTypeArray(data.mimeTypes);
      const plugins = generatePluginArray(data.plugins);

      for (const pluginData of data.plugins) {
        pluginData.__mimeTypes.forEach((type, index) => {
          plugins[pluginData.name][index] = mimeTypes[type];

          Object.defineProperty(plugins[pluginData.name], type, {
            value: mimeTypes[type],
            writable: false,
            enumerable: false,
            configurable: true,
          });

          Object.defineProperty(mimeTypes[type], 'enabledPlugin', {
            value:
              type === 'application/x-pnacl'
                ? mimeTypes['application/x-nacl'].enabledPlugin
                : new Proxy(plugins[pluginData.name], {}),
            writable: false,
            enumerable: false,
            configurable: true,
          });
        });
      }

      const patchNavigator = (name, value) =>
        Object.defineProperty(Object.getPrototypeOf(navigator), name, {
          get() {
            return value;
          },
          configurable: true,
        });

      patchNavigator('mimeTypes', mimeTypes);
      patchNavigator('plugins', plugins);
    }
  } catch (e) {}

  // ============================================================
  // 7. navigator.languages
  // ============================================================
  try {
    Object.defineProperty(navigator, 'languages', {
      get: () => STEALTH_LANGUAGES,
      configurable: true,
    });
  } catch (e) {}

  // ============================================================
  // 8. navigator.vendor
  // ============================================================
  try {
    Object.defineProperty(navigator, 'vendor', {
      get: () => 'Google Inc.',
      configurable: true,
    });
  } catch (e) {}

  // ============================================================
  // 9. navigator.hardwareConcurrency
  // ============================================================
  try {
    Object.defineProperty(navigator, 'hardwareConcurrency', {
      get: () => STEALTH_HARDWARE_CONCURRENCY,
      configurable: true,
    });
  } catch (e) {}

  // ============================================================
  // 10. navigator.permissions (normalize behavior)
  // ============================================================
  try {
    const originalQuery = navigator.permissions?.query?.bind(
      navigator.permissions
    );

    if (originalQuery) {
      navigator.permissions.query = async function (parameters) {
        if (parameters.name === 'notifications') {
          return {
            state: Notification?.permission || 'prompt',
            onchange: null,
            addEventListener: () => {},
            removeEventListener: () => {},
            dispatchEvent: () => true,
          };
        }
        return originalQuery(parameters);
      };
    }

    if (
      typeof Notification !== 'undefined' &&
      window.location.protocol === 'https:'
    ) {
      Object.defineProperty(Notification, 'permission', {
        get: () => 'default',
        configurable: true,
      });
    }
  } catch (e) {}

  // ============================================================
  // 11. WebGL vendor/renderer (avoid SwiftShader detection)
  // ============================================================
  try {
    const getParameter = WebGLRenderingContext.prototype.getParameter;

    WebGLRenderingContext.prototype.getParameter = function (parameter) {
      if (parameter === 37445) {
        return STEALTH_WEBGL_VENDOR;
      }
      if (parameter === 37446) {
        return STEALTH_WEBGL_RENDERER;
      }
      return getParameter.call(this, parameter);
    };

    if (typeof WebGL2RenderingContext !== 'undefined') {
      const getParameter2 = WebGL2RenderingContext.prototype.getParameter;

      WebGL2RenderingContext.prototype.getParameter = function (parameter) {
        if (parameter === 37445) {
          return STEALTH_WEBGL_VENDOR;
        }
        if (parameter === 37446) {
          return STEALTH_WEBGL_RENDERER;
        }
        return getParameter2.call(this, parameter);
      };
    }
  } catch (e) {}

  // ============================================================
  // 12. window.outerWidth/outerHeight (should match inner)
  // ============================================================
  try {
    if (window.outerWidth === 0) {
      Object.defineProperty(window, 'outerWidth', {
        get: () => window.innerWidth,
        configurable: true,
      });
    }
    if (window.outerHeight === 0) {
      Object.defineProperty(window, 'outerHeight', {
        get: () => window.innerHeight,
        configurable: true,
      });
    }
  } catch (e) {}

  // ============================================================
  // 13. iframe.contentWindow (fix cross-origin srcdoc detection)
  // ============================================================
  try {
    const contentWindowDescriptor = Object.getOwnPropertyDescriptor(
      HTMLIFrameElement.prototype,
      'contentWindow'
    );

    if (contentWindowDescriptor && contentWindowDescriptor.get) {
      Object.defineProperty(HTMLIFrameElement.prototype, 'contentWindow', {
        get: function () {
          const result = contentWindowDescriptor.get.call(this);
          if (!result) {
            if (this.srcdoc) {
              return window;
            }
          }
          return result;
        },
        configurable: true,
      });
    }
  } catch (e) {}

  // ============================================================
  // 14. HTMLMediaElement.canPlayType() (realistic codec responses)
  // ============================================================
  try {
    const originalCanPlayType = HTMLMediaElement.prototype.canPlayType;

    HTMLMediaElement.prototype.canPlayType = function (type) {
      if (type.includes('mp4') || type.includes('avc1') || type.includes('mp4a')) {
        if (type.includes('avc1.42E01E') || type.includes('mp4a.40.2')) {
          return 'probably';
        }
        return 'maybe';
      }
      return originalCanPlayType.call(this, type);
    };
  } catch (e) {}

  // ============================================================
  // 15. navigator.deviceMemory
  // ============================================================
  try {
    if (!navigator.deviceMemory) {
      Object.defineProperty(navigator, 'deviceMemory', {
        get: () => STEALTH_DEVICE_MEMORY,
        configurable: true,
      });
    }
  } catch (e) {}

  // ============================================================
  // 16. navigator.connection (Network Information API mock)
  // ============================================================
  try {
    if (!navigator.connection) {
      Object.defineProperty(navigator, 'connection', {
        get: () => ({
          effectiveType: '4g',
          rtt: 50,
          downlink: 10,
          saveData: false,
          addEventListener: () => {},
          removeEventListener: () => {},
          dispatchEvent: () => true,
        }),
        configurable: true,
      });
    }
  } catch (e) {}

  // ============================================================
  // ADDITIONAL EVASIONS
  // ============================================================

  // Battery API mock
  try {
    if (!navigator.getBattery) {
      navigator.getBattery = async () => ({
        charging: true,
        chargingTime: 0,
        dischargingTime: Infinity,
        level: 1,
        addEventListener: () => {},
        removeEventListener: () => {},
        dispatchEvent: () => true,
      });
    }
  } catch (e) {}

  // Screen properties fix (headless reports 0)
  try {
    if (screen.availWidth === 0 || screen.availHeight === 0) {
      Object.defineProperties(screen, {
        availWidth: { get: () => window.innerWidth, configurable: true },
        availHeight: { get: () => window.innerHeight, configurable: true },
        width: { get: () => window.innerWidth, configurable: true },
        height: { get: () => window.innerHeight, configurable: true },
      });
    }
  } catch (e) {}

  // Clipboard API mock
  try {
    if (!navigator.clipboard) {
      Object.defineProperty(navigator, 'clipboard', {
        get: () => ({
          readText: async () => '',
          writeText: async () => {},
          read: async () => [],
          write: async () => {},
        }),
        configurable: true,
      });
    }
  } catch (e) {}

  // Timing jitter — makes setTimeout/setInterval delays less perfectly predictable
  try {
    const originalSetTimeout = window.setTimeout;
    const originalSetInterval = window.setInterval;

    window.setTimeout = function (callback, delay, ...args) {
      const jitter = Math.random() * 5;
      return originalSetTimeout.call(this, callback, delay + jitter, ...args);
    };

    window.setInterval = function (callback, delay, ...args) {
      const jitter = Math.random() * 5;
      return originalSetInterval.call(this, callback, delay + jitter, ...args);
    };
  } catch (e) {}

  // Date.now() micro-jitter
  try {
    const originalDateNow = Date.now;
    let lastNow = originalDateNow();

    Date.now = function () {
      const now = originalDateNow();
      lastNow = Math.max(lastNow + 1, now + Math.random() * 0.1);
      return Math.floor(lastNow);
    };
  } catch (e) {}
})();
