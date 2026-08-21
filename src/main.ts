import "./styles.css";

const root = document.querySelector<HTMLElement>("#app");

if (root === null) {
  throw new Error("缺少 #app 根节点");
}

root.innerHTML = `
  <main class="boot" aria-live="polite">
    <img src="/icon-128.png" alt="" width="96" height="96" />
    <h1>DSH Desktop</h1>
    <p>正在启动 DSH…</p>
  </main>
`;
