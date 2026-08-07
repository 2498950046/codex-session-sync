import { useEffect, type ReactNode } from "react";
import { NavLink, useLocation } from "./router";
import {
  Activity,
  ChevronRight,
  CloudCog,
  Home,
  Menu,
  MessagesSquare,
  RefreshCw,
  Settings,
  SlidersHorizontal,
} from "lucide-react";
import iconUrl from "../app-icon.svg";
import type { CodexProcess } from "./types";

const navigation = [
  { to: "/overview", label: "概览", icon: Home },
  { to: "/sync", label: "同步", icon: RefreshCw },
  { to: "/sessions", label: "会话", icon: MessagesSquare },
  { to: "/settings", label: "设置", icon: Settings },
] as const;

const titles: Record<string, { title: string; description: string }> = {
  "/overview": { title: "概览", description: "查看同步环境、配置状态和最近结果" },
  "/sync": { title: "同步", description: "安全推送、拉取或切换 Codex 会话" },
  "/sessions": { title: "会话", description: "扫描本机会话并检查兼容性" },
  "/settings": { title: "设置", description: "配置本机路径、远端服务器、命名空间和高级工具" },
};

function currentTitle(pathname: string) {
  const key = Object.keys(titles).find((candidate) => pathname.startsWith(candidate)) ?? "/overview";
  return titles[key];
}

type AppShellProps = {
  children: ReactNode;
  processes: CodexProcess[];
  busy: boolean;
  onRefreshProcesses: () => void;
};

export function AppShell({ children, processes, busy, onRefreshProcesses }: AppShellProps) {
  const location = useLocation();
  const heading = currentTitle(location.pathname);

  useEffect(() => {
    window.localStorage.setItem("codex-session-sync.last-route", location.pathname);
  }, [location.pathname]);

  return <div className="application-shell">
    <aside className="sidebar">
      <div className="sidebar-brand">
        <img src={iconUrl} alt="" />
        <div><strong>Codex Session Sync</strong><span>安全同步会话</span></div>
      </div>
      <nav className="sidebar-navigation" aria-label="主导航">
        {navigation.map(({ to, label, icon: Icon }) => <NavLink
          key={to}
          to={to}
          className={({ isActive }) => `navigation-item ${isActive ? "active" : ""}`}
          title={label}
        >
          <Icon size={19} aria-hidden="true" />
          <span>{label}</span>
          <ChevronRight className="navigation-arrow" size={15} aria-hidden="true" />
        </NavLink>)}
      </nav>
      <div className="sidebar-footer">
        <SlidersHorizontal size={17} aria-hidden="true" />
        <span>酱鱼2498950046@qq.com</span>
      </div>
    </aside>

    <div className="application-main">
      <header className="topbar">
        <div className="topbar-title">
          <Menu className="compact-menu-mark" size={19} aria-hidden="true" />
          <div><h1>{heading.title}</h1><p>{heading.description}</p></div>
        </div>
        <div className="topbar-actions">
          <button
            type="button"
            className={`runtime-status ${processes.length ? "warning" : "ready"}`}
            onClick={onRefreshProcesses}
            disabled={busy}
            title="重新检测 Codex 运行状态"
          >
            {processes.length ? <Activity size={16} /> : <CloudCog size={16} />}
            <span>{processes.length ? `Codex 运行中 · ${processes.length}` : "Codex 已退出"}</span>
            <RefreshCw size={14} aria-hidden="true" />
          </button>
        </div>
      </header>
      <main className="page-content">{children}</main>
    </div>
  </div>;
}
