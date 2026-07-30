import {
  Children,
  createContext,
  isValidElement,
  useContext,
  useEffect,
  useMemo,
  useState,
  type AnchorHTMLAttributes,
  type ReactElement,
  type ReactNode,
} from "react";

type NavigateOptions = { replace?: boolean };
type RouterContextValue = {
  pathname: string;
  navigate: (to: string, options?: NavigateOptions) => void;
};

const RouterContext = createContext<RouterContextValue | null>(null);

function normalizedPath(path: string) {
  const withoutQuery = path.split("?", 1)[0] || "/";
  return withoutQuery.startsWith("/") ? withoutQuery : `/${withoutQuery}`;
}

function hashPath() {
  return normalizedPath(window.location.hash.replace(/^#/, "") || "/");
}

export function HashRouter({ children }: { children: ReactNode }) {
  const [pathname, setPathname] = useState(hashPath);

  useEffect(() => {
    const update = () => setPathname(hashPath());
    window.addEventListener("hashchange", update);
    return () => window.removeEventListener("hashchange", update);
  }, []);

  const value = useMemo<RouterContextValue>(() => ({
    pathname,
    navigate(to, options) {
      const next = normalizedPath(to);
      if (options?.replace) {
        window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}#${next}`);
        setPathname(next);
      } else if (hashPath() !== next) {
        window.location.hash = next;
      }
    },
  }), [pathname]);

  return <RouterContext.Provider value={value}>{children}</RouterContext.Provider>;
}

export function MemoryRouter({ children, initialEntries = ["/"] }: { children: ReactNode; initialEntries?: string[] }) {
  const [pathname, setPathname] = useState(() => normalizedPath(initialEntries[0] ?? "/"));
  const value = useMemo<RouterContextValue>(() => ({
    pathname,
    navigate(to) { setPathname(normalizedPath(to)); },
  }), [pathname]);
  return <RouterContext.Provider value={value}>{children}</RouterContext.Provider>;
}

export function useLocation() {
  const router = useContext(RouterContext);
  if (!router) throw new Error("useLocation must be used inside a router");
  return { pathname: router.pathname };
}

export function useNavigate() {
  const router = useContext(RouterContext);
  if (!router) throw new Error("useNavigate must be used inside a router");
  return router.navigate;
}

type NavLinkProps = Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "className" | "href"> & {
  to: string;
  className?: string | ((state: { isActive: boolean }) => string);
};

export function NavLink({ to, className, onClick, children, ...props }: NavLinkProps) {
  const router = useContext(RouterContext);
  if (!router) throw new Error("NavLink must be used inside a router");
  const target = normalizedPath(to);
  const isActive = router.pathname === target || (target !== "/" && router.pathname.startsWith(`${target}/`));
  return <a
    {...props}
    href={`#${target}`}
    className={typeof className === "function" ? className({ isActive }) : className}
    onClick={(event) => {
      onClick?.(event);
      if (!event.defaultPrevented && event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey) {
        event.preventDefault();
        router.navigate(target);
      }
    }}
  >{children}</a>;
}

type RouteProps = { path: string; element: ReactNode };
export function Route(_props: RouteProps) { return null; }

function matches(pattern: string, pathname: string) {
  if (pattern === "*") return true;
  if (pattern.endsWith("/*")) {
    const base = normalizedPath(pattern.slice(0, -2));
    return pathname === base || pathname.startsWith(`${base}/`);
  }
  return normalizedPath(pattern) === pathname;
}

export function Routes({ children }: { children: ReactNode }) {
  const { pathname } = useLocation();
  for (const child of Children.toArray(children)) {
    if (!isValidElement<RouteProps>(child) || child.type !== Route) continue;
    if (matches(child.props.path, pathname)) return child.props.element;
  }
  return null;
}

export function Navigate({ to, replace = false }: { to: string; replace?: boolean }) {
  const navigate = useNavigate();
  useEffect(() => navigate(to, { replace }), [navigate, replace, to]);
  return null;
}

export type RouteElement = ReactElement<RouteProps>;
