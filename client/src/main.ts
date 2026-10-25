import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import { router } from "./router";
import { applyThemeClass, useThemeStore } from "./stores/theme";
import "./styles/global.css";

const pinia = createPinia();
const app = createApp(App);
app.use(pinia).use(router);

// 挂载前先落主题类名：避免首帧闪过默认色（本轮默认亮色）。
applyThemeClass(useThemeStore(pinia).mode);

app.mount("#app");
