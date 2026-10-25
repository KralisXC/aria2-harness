import { defineAsyncComponent } from "vue";
import { createRouter, createWebHashHistory } from "vue-router";
import DashboardView from "./views/DashboardView.vue";
import DownloadsView from "./views/DownloadsView.vue";

/**
 * 非核心页面全部懒加载：首屏只下载 dashboard / downloads 两个 chunk，
 * 设置、插件、Tracker、通知在首次导航时才拉取，缩短二次启动的白屏时间。
 */
const NotificationsView = defineAsyncComponent(
  () => import("./views/NotificationsView.vue"),
);
const PluginsView = defineAsyncComponent(() => import("./views/PluginsView.vue"));
const TrackersView = defineAsyncComponent(() => import("./views/TrackersView.vue"));

/**
 * 视图路由（增量 24：为高级 UI 铺路的前端拆分）。
 *
 * 必须用 `createWebHashHistory`：Tauri 下页面由自定义协议加载、没有服务端，
 * `createWebHistory` 的 pushState 路径在刷新/直达时会 404。
 */
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    { path: "/", redirect: "/dashboard" },
    { path: "/dashboard", name: "dashboard", component: DashboardView },
    { path: "/downloads", name: "downloads", component: DownloadsView },
    { path: "/trackers", name: "trackers", component: TrackersView },
    { path: "/plugins", name: "plugins", component: PluginsView },
    { path: "/notifications", name: "notifications", component: NotificationsView },
    // beta7：设置已改为弹窗（由 App.vue 常驻渲染），不再是独立路由页面。
    { path: "/:pathMatch(.*)*", redirect: "/dashboard" },
  ],
});
