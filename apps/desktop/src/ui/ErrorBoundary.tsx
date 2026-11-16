import { Component, type ReactNode } from "react";
import { CapsuleButton } from "./CapsuleButton";

/** Bir render hatası tüm pencereyi boş bırakmasın; yeniden deneme alt ağacı yeniden kurar. */
export class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: unknown) {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <main className="ui-error-screen">
        <section className="ui-error-screen__content" role="alert" aria-labelledby="render-error-title">
          <h1 id="render-error-title">Ultron görüntülenemedi</h1>
          <p>Beklenmeyen bir hata oluştu. Yeniden deneyebilirsin.</p>
          <pre>{this.state.error.message}</pre>
          <CapsuleButton onClick={() => this.setState({ error: null })}>Yeniden dene</CapsuleButton>
        </section>
      </main>
    );
  }
}
