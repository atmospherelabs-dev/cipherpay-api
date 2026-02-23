/**
 * CipherPay Checkout Widget
 * Embeddable shielded Zcash payment widget.
 *
 * Usage:
 *   <div id="cipherpay" data-invoice-id="UUID" data-api="https://pay.cipherscan.app"></div>
 *   <script src="https://pay.cipherscan.app/widget/cipherpay.js"></script>
 */
(function () {
  'use strict';

  const POLL_INTERVAL = 5000;
  const STATUS_LABELS = {
    pending: 'Waiting for payment...',
    underpaid: 'Partial payment received. Send remaining balance.',
    detected: 'Payment detected! Confirming...',
    confirmed: 'Payment confirmed!',
    expired: 'Invoice expired',
    refunded: 'Payment refunded',
  };

  function loadStyles(apiUrl) {
    if (document.getElementById('cipherpay-styles')) return;
    var link = document.createElement('link');
    link.id = 'cipherpay-styles';
    link.rel = 'stylesheet';
    link.href = apiUrl + '/widget/cipherpay.css';
    document.head.appendChild(link);
  }

  function formatTime(seconds) {
    var m = Math.floor(seconds / 60);
    var s = seconds % 60;
    return m + ':' + (s < 10 ? '0' : '') + s;
  }

  function formatZec(amount) {
    return parseFloat(amount).toFixed(4);
  }

  async function fetchInvoice(apiUrl, invoiceId) {
    var resp = await fetch(apiUrl + '/api/invoices/' + invoiceId);
    if (!resp.ok) throw new Error('Failed to fetch invoice');
    return resp.json();
  }

  async function fetchStatus(apiUrl, invoiceId) {
    var resp = await fetch(apiUrl + '/api/invoices/' + invoiceId + '/status');
    if (!resp.ok) throw new Error('Failed to fetch status');
    return resp.json();
  }

  function renderWidget(container, invoice) {
    var expiresAt = new Date(invoice.expires_at);
    var now = new Date();
    var remainingSecs = Math.max(0, Math.floor((expiresAt - now) / 1000));

    container.innerHTML = '';
    var widget = document.createElement('div');
    widget.className = 'cipherpay-widget';

    widget.innerHTML =
      '<div class="cipherpay-header">' +
        '<div class="cipherpay-logo">CIPHERPAY</div>' +
        '<div class="cipherpay-network">SHIELDED ZEC</div>' +
      '</div>' +

      '<div class="cipherpay-amount">' +
        '<div class="cipherpay-amount-zec">' + formatZec(invoice.price_zec) + '<span>ZEC</span></div>' +
        '<div class="cipherpay-amount-fiat">' + parseFloat(invoice.price_eur).toFixed(2) + ' EUR</div>' +
      '</div>' +

      '<div class="cipherpay-qr" id="cipherpay-qr"></div>' +

      '<div class="cipherpay-memo">' +
        '<div class="cipherpay-memo-label">Include this memo</div>' +
        '<div class="cipherpay-memo-code" title="Click to copy">' + invoice.memo_code + '</div>' +
      '</div>' +

      '<div class="cipherpay-timer">' +
        'Rate valid for <span class="cipherpay-timer-value" id="cipherpay-timer">' +
        formatTime(remainingSecs) + '</span>' +
      '</div>' +

      '<div class="cipherpay-status cipherpay-status-' + invoice.status + '" id="cipherpay-status">' +
        STATUS_LABELS[invoice.status] +
      '</div>' +

      '<div class="cipherpay-footer">' +
        'Powered by <a href="https://cipherscan.app" target="_blank">CipherScan</a>' +
      '</div>';

    container.appendChild(widget);

    // Copy memo on click
    var memoEl = widget.querySelector('.cipherpay-memo-code');
    if (memoEl) {
      memoEl.addEventListener('click', function () {
        navigator.clipboard.writeText(invoice.memo_code).then(function () {
          memoEl.textContent = 'Copied!';
          setTimeout(function () {
            memoEl.textContent = invoice.memo_code;
          }, 1500);
        });
      });
    }

    // Countdown timer
    var timerEl = document.getElementById('cipherpay-timer');
    if (timerEl && remainingSecs > 0) {
      var timerInterval = setInterval(function () {
        remainingSecs--;
        if (remainingSecs <= 0) {
          clearInterval(timerInterval);
          timerEl.textContent = 'Expired';
          return;
        }
        timerEl.textContent = formatTime(remainingSecs);
      }, 1000);
    }

    return widget;
  }

  function updateStatus(widget, status) {
    var el = document.getElementById('cipherpay-status');
    if (!el) return;
    el.className = 'cipherpay-status cipherpay-status-' + status;
    el.innerHTML = STATUS_LABELS[status] || status;
  }

  async function init() {
    var container = document.getElementById('cipherpay');
    if (!container) return;

    var invoiceId = container.getAttribute('data-invoice-id');
    var apiUrl = container.getAttribute('data-api') || '';

    if (!invoiceId) {
      container.innerHTML = '<div style="color:#FF6B35;font-family:monospace;font-size:12px">CipherPay: missing data-invoice-id</div>';
      return;
    }

    loadStyles(apiUrl);

    try {
      var invoice = await fetchInvoice(apiUrl, invoiceId);
      var widget = renderWidget(container, invoice);

      if (invoice.status === 'pending' || invoice.status === 'detected' || invoice.status === 'underpaid') {
        var pollInterval = setInterval(async function () {
          try {
            var statusResp = await fetchStatus(apiUrl, invoiceId);
            if (statusResp.status !== invoice.status) {
              invoice.status = statusResp.status;
              updateStatus(widget, statusResp.status);

              if (statusResp.status === 'confirmed' || statusResp.status === 'expired') {
                clearInterval(pollInterval);
              }
            }
          } catch (e) {
            // Silent retry
          }
        }, POLL_INTERVAL);
      }
    } catch (e) {
      container.innerHTML = '<div style="color:#FF6B35;font-family:monospace;font-size:12px">CipherPay: ' + e.message + '</div>';
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
