import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
  info: ErrorInfo | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, info: null };

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    this.setState({ error, info });
    console.error("Render crash caught by ErrorBoundary:", error, info.componentStack);
  }

  render() {
    if (this.state.error) {
      return (
        <pre style={{ whiteSpace: "pre-wrap", padding: "1rem", color: "#d32f2f", textAlign: "left" }}>
          {this.state.error.name}: {this.state.error.message}
          {"\n\n"}
          {this.state.error.stack}
          {this.state.info?.componentStack}
        </pre>
      );
    }
    return this.props.children;
  }
}
