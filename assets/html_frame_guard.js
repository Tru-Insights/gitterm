(() => {
  for (const key of ['RTCPeerConnection', 'webkitRTCPeerConnection']) {
    Object.defineProperty(window, key, { value: undefined, writable: false, configurable: false });
  }
  Object.defineProperty(window, '__gittermHtmlGuard', { value: true, writable: false, configurable: false });
})();
