import { Link } from "react-router-dom";
import { EmptyState, Page, buttonClass } from "../components/ui";

export function NotFound() {
  return (
    <Page>
      <EmptyState
        title="Page not found"
        action={
          <Link to="/" className={buttonClass()}>
            Overview
          </Link>
        }
      />
    </Page>
  );
}
